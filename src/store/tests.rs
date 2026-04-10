use super::*;
use std::sync::Arc;

#[tokio::test]
async fn create_and_get_task() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test task".to_string(),
            body: "Test body".to_string(),
            source: "cron".to_string(),
            source_id: "daily-sync".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    assert_eq!(id, 1);

    let task = store.get(id).await.unwrap();
    assert_eq!(task.title, "Test task");
    assert_eq!(task.body, "Test body");
    assert_eq!(task.status, TaskStatus::New);
    assert_eq!(task.origin, "internal");
    assert_eq!(task.repo, "owner/repo");
}

#[tokio::test]
async fn upsert_external_task() {
    let store = TaskStore::open_memory().await.unwrap();

    let id1 = store
        .upsert_external(&UpsertExternal {
            repo: "owner/repo",
            ext_id: "42",
            title: "Original title",
            body: "Original body",
            author: "user",
            url: "https://github.com/owner/repo/issues/42",
            labels: &["bug".to_string()],
            origin: "github",
        })
        .await
        .unwrap();

    // Upsert same external_id — should update, not create new
    let id2 = store
        .upsert_external(&UpsertExternal {
            repo: "owner/repo",
            ext_id: "42",
            title: "Updated title",
            body: "Updated body",
            author: "user",
            url: "https://github.com/owner/repo/issues/42",
            labels: &["bug".to_string(), "priority:high".to_string()],
            origin: "github",
        })
        .await
        .unwrap();

    assert_eq!(id1, id2);

    let task = store.get(id1).await.unwrap();
    assert_eq!(task.title, "Updated title");
    assert_eq!(task.external_id, Some("42".to_string()));
}

/// Re-ingesting an existing external task must NOT reset updated_at.
///
/// The sync catch-up path in sync.rs uses `updated_at` as a proxy for
/// "time since last status change". If upsert_external resets it on every
/// periodic re-ingest (every 45s), NeedsReview tasks never appear stale
/// and the catch-up event is never re-fired (issue #892).
#[tokio::test]
async fn upsert_external_does_not_reset_updated_at_on_reingest() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .upsert_external(&UpsertExternal {
            repo: "owner/repo",
            ext_id: "99",
            title: "Original",
            body: "body",
            author: "user",
            url: "https://github.com/owner/repo/issues/99",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();

    // Backdate updated_at to simulate a task that entered NeedsReview 10 minutes ago.
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(&store.pool)
        .await
        .unwrap();

    // Re-ingest the same external task (simulates periodic ingest_external_tasks).
    store
        .upsert_external(&UpsertExternal {
            repo: "owner/repo",
            ext_id: "99",
            title: "Original",
            body: "body",
            author: "user",
            url: "https://github.com/owner/repo/issues/99",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.updated_at, "2020-01-01T00:00:00Z",
        "upsert_external must not reset updated_at on re-ingest — staleness checks rely on it"
    );
}

#[tokio::test]
async fn update_status() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    store
        .update_status(id, TaskStatus::InProgress)
        .await
        .unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::InProgress);
}

#[tokio::test]
async fn list_by_status() {
    let store = TaskStore::open_memory().await.unwrap();

    for i in 0..3 {
        store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: format!("Task {i}"),
                body: "".to_string(),
                source: "cron".to_string(),
                source_id: format!("job-{i}"),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
                parent_id: None,
            })
            .await
            .unwrap();
    }

    let new_tasks = store
        .list_by_status("owner/repo", TaskStatus::New)
        .await
        .unwrap();
    assert_eq!(new_tasks.len(), 3);

    store.update_status(1, TaskStatus::Done).await.unwrap();
    let done = store
        .list_by_status("owner/repo", TaskStatus::Done)
        .await
        .unwrap();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].title, "Task 0");
}

#[tokio::test]
async fn increment_and_reset_counters() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    let v = store.increment(id, "attempts").await.unwrap();
    assert_eq!(v, 1);
    let v = store.increment(id, "attempts").await.unwrap();
    assert_eq!(v, 2);

    store.reset_counters(id).await.unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(task.attempts, 0);
}

#[tokio::test]
async fn increment_propagates_returned_value_decode_errors() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    store
        .set_fields(id, &[("push_failures", serde_json::json!(i32::MAX))])
        .await
        .unwrap();
    let err = store.increment(id, "push_failures").await.unwrap_err();
    assert!(
        err.to_string().contains("out of range") || err.to_string().contains("decode"),
        "unexpected error: {err:#}"
    );
}

#[tokio::test]
async fn helper_get_token_usage_from_store() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let id = store
        .create(&NewTask {
            external_id: Some("70".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Token test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    store
        .set_fields(
            id,
            &[
                ("input_tokens", serde_json::json!(1500)),
                ("output_tokens", serde_json::json!(800)),
            ],
        )
        .await
        .unwrap();

    let opt_store = Some(store);
    let usage = get_token_usage(&opt_store, "owner/repo", "70").await;
    assert_eq!(usage.input_tokens, 1500);
    assert_eq!(usage.output_tokens, 800);
    assert_eq!(usage.total_tokens(), 2300);
}

#[tokio::test]
async fn helper_get_cost_estimate_from_store() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let id = store
        .create(&NewTask {
            external_id: Some("71".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Cost test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    store
        .set_fields(
            id,
            &[
                ("input_cost_usd", serde_json::json!(0.05)),
                ("output_cost_usd", serde_json::json!(0.10)),
                ("total_cost_usd", serde_json::json!(0.15)),
            ],
        )
        .await
        .unwrap();

    let opt_store = Some(store);
    let cost = get_cost_estimate(&opt_store, "owner/repo", "71").await;
    assert!((cost.input_cost_usd - 0.05).abs() < f64::EPSILON);
    assert!((cost.output_cost_usd - 0.10).abs() < f64::EPSILON);
    assert!((cost.total_cost_usd - 0.15).abs() < f64::EPSILON);
}

#[tokio::test]
async fn helper_get_recent_memory_from_store() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let id = store
        .create(&NewTask {
            external_id: Some("72".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Memory test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let entry1 = MemoryEntry {
        attempt: 1,
        agent: "claude".to_string(),
        model: Some("opus".to_string()),
        learnings: vec!["learned A".to_string()],
        error: None,
        files_modified: vec!["src/main.rs".to_string()],
        approach: "first try".to_string(),
        timestamp: "2026-01-01T00:00:00Z".to_string(),
    };
    let entry2 = MemoryEntry {
        attempt: 2,
        agent: "codex".to_string(),
        model: Some("gpt-5".to_string()),
        learnings: vec!["learned B".to_string()],
        error: Some("timeout".to_string()),
        files_modified: vec![],
        approach: "second try".to_string(),
        timestamp: "2026-01-01T01:00:00Z".to_string(),
    };

    store.append_memory(id, &entry1).await.unwrap();
    store.append_memory(id, &entry2).await.unwrap();

    let opt_store = Some(store);
    let memory = get_recent_memory(&opt_store, "owner/repo", "72", 10).await;
    assert_eq!(memory.len(), 2);
    assert_eq!(memory[0].attempt, 1);
    assert_eq!(memory[1].attempt, 2);
    assert_eq!(memory[0].agent, "claude");
    assert_eq!(memory[1].agent, "codex");
}

#[tokio::test]
async fn helper_store_increment_returns_new_value() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let id = store
        .create(&NewTask {
            external_id: Some("73".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Increment test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let opt_store = Some(store);
    let v1 = store_increment(&opt_store, "owner/repo", "73", "attempts")
        .await
        .unwrap();
    assert_eq!(v1, 1);

    let v2 = store_increment(&opt_store, "owner/repo", "73", "attempts")
        .await
        .unwrap();
    assert_eq!(v2, 2);

    let v3 = store_increment(&opt_store, "owner/repo", "73", "attempts")
        .await
        .unwrap();
    assert_eq!(v3, 3);

    let task = opt_store.as_ref().unwrap().get(id).await.unwrap();
    assert_eq!(task.attempts, 3);
}

#[tokio::test]
async fn helper_store_increment_without_store_returns_zero() {
    let store: Option<Arc<TaskStore>> = None;
    let res = store_increment(&store, "owner/repo", "no-store-task", "attempts").await;
    assert!(res.is_err());
}

#[tokio::test]
async fn helper_store_set_with_none_store() {
    let store: Option<Arc<TaskStore>> = None;
    store_set(
        &store,
        "owner/repo",
        "42",
        &[("branch", serde_json::json!("main"))],
    )
    .await;
}

#[tokio::test]
async fn helper_store_set_writes_fields() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let id = store
        .create(&NewTask {
            external_id: Some("90".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Store set test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let opt_store = Some(store.clone());
    store_set(
        &opt_store,
        "owner/repo",
        "90",
        &[
            ("branch", serde_json::json!("fix-bug")),
            ("worktree", serde_json::json!("/tmp/wt")),
        ],
    )
    .await;

    let task = store.get(id).await.unwrap();
    assert_eq!(task.branch, "fix-bug");
    assert_eq!(task.worktree, "/tmp/wt");
}

#[tokio::test]
async fn helper_store_set_by_id_writes_fields() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let id = store
        .create(&NewTask {
            external_id: Some("90b".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Store set by id test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let opt_store = Some(store.clone());
    store_set_by_id(
        &opt_store,
        id,
        &[
            ("branch", serde_json::json!("fix-bug-by-id")),
            ("worktree", serde_json::json!("/tmp/wt-by-id")),
        ],
    )
    .await;

    let task = store.get(id).await.unwrap();
    assert_eq!(task.branch, "fix-bug-by-id");
    assert_eq!(task.worktree, "/tmp/wt-by-id");
}

#[tokio::test]
async fn helper_store_set_ignores_unknown_task() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let opt_store = Some(store);
    store_set(
        &opt_store,
        "owner/repo",
        "nonexistent-999",
        &[("branch", serde_json::json!("main"))],
    )
    .await;
}

#[tokio::test]
async fn helper_store_reset_counters_zeroes_counters() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let id = store
        .create(&NewTask {
            external_id: Some("91".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Reset test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let opt_store = Some(store.clone());
    store_increment(&opt_store, "owner/repo", "91", "attempts")
        .await
        .unwrap();
    store_increment(&opt_store, "owner/repo", "91", "attempts")
        .await
        .unwrap();
    store_increment(&opt_store, "owner/repo", "91", "merge_conflict_retries")
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.attempts, 2);
    assert_eq!(task.merge_conflict_retries, 1);

    store_reset_counters(&opt_store, "owner/repo", "91").await;

    let task = store.get(id).await.unwrap();
    assert_eq!(task.attempts, 0);
    assert_eq!(task.merge_conflict_retries, 0);
}

#[tokio::test]
async fn helper_store_reset_counters_noop_without_store() {
    let store: Option<Arc<TaskStore>> = None;
    store_reset_counters(&store, "owner/repo", "no-task").await;
}

#[tokio::test]
async fn helper_store_reset_failure_counters_preserves_review_cycles() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let id = store
        .create(&NewTask {
            external_id: Some("93".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Failure counter test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let opt_store = Some(store.clone());
    store_increment(&opt_store, "owner/repo", "93", "review_cycles")
        .await
        .unwrap();
    store_increment(&opt_store, "owner/repo", "93", "review_invocations")
        .await
        .unwrap();
    store_increment(&opt_store, "owner/repo", "93", "attempts")
        .await
        .unwrap();
    store_increment(&opt_store, "owner/repo", "93", "merge_conflict_retries")
        .await
        .unwrap();

    store_reset_failure_counters(&opt_store, "owner/repo", "93").await;

    let task = store.get(id).await.unwrap();
    assert_eq!(task.review_cycles, 1);
    assert_eq!(task.review_invocations, 0);
    assert_eq!(task.attempts, 1, "attempts must be preserved (monotonic)");
    assert_eq!(task.merge_conflict_retries, 0);
}

#[tokio::test]
async fn helper_store_increment_by_id_returns_new_value() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let id = store
        .create(&NewTask {
            external_id: Some("73b".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Increment by id test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let opt_store = Some(store.clone());
    let v1 = store_increment_by_id(&opt_store, id, "attempts")
        .await
        .unwrap();
    let v2 = store_increment_by_id(&opt_store, id, "attempts")
        .await
        .unwrap();

    assert_eq!(v1, 1);
    assert_eq!(v2, 2);

    let task = store.get(id).await.unwrap();
    assert_eq!(task.attempts, 2);
}

#[tokio::test]
async fn helper_get_total_tokens_sums_input_and_output() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    store
        .create(&NewTask {
            external_id: Some("92".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Tokens test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    store
        .set_fields(
            1,
            &[
                ("input_tokens", serde_json::json!(5000)),
                ("output_tokens", serde_json::json!(3000)),
            ],
        )
        .await
        .unwrap();

    let opt_store = Some(store);
    let total = get_total_tokens(&opt_store, "owner/repo", "92").await;
    assert_eq!(total, 8000);
}

#[tokio::test]
async fn helper_get_total_tokens_returns_zero_without_store() {
    let store: Option<Arc<TaskStore>> = None;
    let total = get_total_tokens(&store, "owner/repo", "any").await;
    assert_eq!(total, 0);
}

#[tokio::test]
async fn helper_get_recent_memory_returns_empty_without_store() {
    let store: Option<Arc<TaskStore>> = None;
    let memory = get_recent_memory(&store, "owner/repo", "any", 10).await;
    assert!(memory.is_empty());
}

#[tokio::test]
async fn memory_append_and_recent() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    for i in 1..=5 {
        store
            .append_memory(
                id,
                &MemoryEntry {
                    attempt: i,
                    agent: "claude".to_string(),
                    model: Some("sonnet".to_string()),
                    learnings: vec![format!("Learning {i}")],
                    error: None,
                    files_modified: vec![],
                    approach: format!("Approach {i}"),
                    timestamp: format!("2026-01-0{i}T00:00:00Z"),
                },
            )
            .await
            .unwrap();
    }

    let recent = store.recent_memory(id, 3).await.unwrap();
    assert_eq!(recent.len(), 3);
    assert_eq!(recent[0].attempt, 3);
    assert_eq!(recent[2].attempt, 5);
}

#[tokio::test]
async fn task_runs_lifecycle() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    let run_id = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "claude -p ...",
            prompt: "system prompt",
        })
        .await
        .unwrap();

    store
        .complete_run(&CompleteRun {
            run_id,
            exit_code: Some(0),
            stdout: "agent output here",
            stderr: "",
            parsed: r#"{"summary":"fixed it"}"#,
            outcome: "success",
            error: "",
            tokens: RunTokenUsage {
                input_tokens: 50000,
                output_tokens: 10000,
                total_cost_usd: 0.30,
                duration_secs: 45.0,
            },
        })
        .await
        .unwrap();

    let runs = store.get_runs(task_id).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome, "success");
    assert_eq!(runs[0].exit_code, Some(0));

    let last = store.get_last_run(task_id, "agent").await.unwrap();
    assert!(last.is_some());
    assert_eq!(last.unwrap().agent, "claude");

    let none = store.get_last_run(task_id, "review").await.unwrap();
    assert!(none.is_none());
}

#[tokio::test]
async fn set_fields_updates_task() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    store
        .set_fields(
            id,
            &[
                ("agent", serde_json::json!("claude")),
                ("branch", serde_json::json!("fix-bug-42")),
                ("pr_number", serde_json::json!(123)),
            ],
        )
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.agent, Some("claude".to_string()));
    assert_eq!(task.branch, "fix-bug-42");
    assert_eq!(task.pr_number, Some(123));
}

#[tokio::test]
async fn set_fields_rejects_unknown_column() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    let result = store
        .set_fields(id, &[("evil_column", serde_json::json!("drop table"))])
        .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn store_tokens() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    store
        .store_tokens(id, 50000, 10000, "sonnet")
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.input_tokens, 50000);
    assert_eq!(task.output_tokens, 10000);
    assert!(task.total_cost_usd > 0.0);
    assert_eq!(task.model, Some("sonnet".to_string()));
}

#[tokio::test]
async fn list_cleanable() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Done task".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // Set worktree and mark done
    store
        .set_fields(id, &[("worktree", serde_json::json!("/tmp/wt"))])
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Done).await.unwrap();

    let cleanable = store.list_cleanable("owner/repo").await.unwrap();
    assert_eq!(cleanable.len(), 1);

    store.mark_cleaned(id).await.unwrap();
    let cleanable = store.list_cleanable("owner/repo").await.unwrap();
    assert_eq!(cleanable.len(), 0);
}

// ---------------------------------------------------------------
// Additional coverage
// ---------------------------------------------------------------

#[tokio::test]
async fn get_nonexistent_task_returns_error() {
    let store = TaskStore::open_memory().await.unwrap();
    let result = store.get(999).await;
    assert!(result.is_err());
    assert!(
        result.unwrap_err().to_string().contains("not found"),
        "error should mention 'not found'"
    );
}

#[tokio::test]
async fn get_by_external_id_returns_none_for_missing() {
    let store = TaskStore::open_memory().await.unwrap();
    let result = store.get_by_external_id("owner/repo", "999").await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn get_by_external_id_finds_existing() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .upsert_external(&UpsertExternal {
            repo: "owner/repo",
            ext_id: "55",
            title: "Find me",
            body: "",
            author: "user",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();

    let task = store
        .get_by_external_id("owner/repo", "55")
        .await
        .unwrap()
        .expect("should find the task");
    assert_eq!(task.title, "Find me");
    assert_eq!(task.external_id, Some("55".to_string()));
}

#[tokio::test]
async fn unique_constraint_on_repo_external_id() {
    let store = TaskStore::open_memory().await.unwrap();

    // Two tasks with same external_id but different repos should both succeed
    let id1 = store
        .upsert_external(&UpsertExternal {
            repo: "owner/repo-a",
            ext_id: "1",
            title: "Task A",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();

    let id2 = store
        .upsert_external(&UpsertExternal {
            repo: "owner/repo-b",
            ext_id: "1",
            title: "Task B",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();

    assert_ne!(id1, id2, "different repos should produce different IDs");
}

#[tokio::test]
async fn list_by_status_scopes_to_repo() {
    let store = TaskStore::open_memory().await.unwrap();

    store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo-a".to_string(),
            origin: "internal".to_string(),
            title: "Task A".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo-b".to_string(),
            origin: "internal".to_string(),
            title: "Task B".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    let a_tasks = store
        .list_by_status("owner/repo-a", TaskStatus::New)
        .await
        .unwrap();
    assert_eq!(a_tasks.len(), 1);
    assert_eq!(a_tasks[0].title, "Task A");

    let b_tasks = store
        .list_by_status("owner/repo-b", TaskStatus::New)
        .await
        .unwrap();
    assert_eq!(b_tasks.len(), 1);
    assert_eq!(b_tasks[0].title, "Task B");
}

#[tokio::test]
async fn set_fields_empty_is_noop() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // Empty updates should succeed without error
    store.set_fields(id, &[]).await.unwrap();
}

#[tokio::test]
async fn set_fields_with_null_value() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // Set agent, then clear it with null
    store
        .set_fields(
            id,
            &[
                ("agent", serde_json::json!("claude")),
                ("summary", serde_json::json!(null)),
            ],
        )
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.agent, Some("claude".to_string()));
}

#[tokio::test]
async fn increment_rejects_disallowed_field() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    let result = store.increment(id, "input_tokens").await;
    assert!(result.is_err(), "input_tokens should not be incrementable");
}

#[tokio::test]
async fn store_route_updates_routing_fields() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Route me".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    store
        .store_route(&StoreRoute {
            id,
            agent: "codex",
            model: Some("gpt-5.2"),
            complexity: "complex",
            estimate: 8,
            reason: "needs deep analysis",
            profile: r#"{"role":"backend"}"#,
            skills: "git,rust",
        })
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.agent, Some("codex".to_string()));
    assert_eq!(task.model, Some("gpt-5.2".to_string()));
    assert_eq!(task.complexity, "complex");
    assert_eq!(task.estimate, 8);
    assert_eq!(task.route_reason, "needs deep analysis");
    assert_eq!(task.selected_skills, "git,rust");
}

#[tokio::test]
async fn memory_empty_by_default() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    let memory = store.recent_memory(id, 10).await.unwrap();
    assert!(memory.is_empty());
}

#[tokio::test]
async fn labels_roundtrip_as_json() {
    let store = TaskStore::open_memory().await.unwrap();
    let labels = vec![
        "status:new".to_string(),
        "agent:claude".to_string(),
        "priority:high".to_string(),
    ];

    let id = store
        .create(&NewTask {
            external_id: Some("10".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Labeled task".to_string(),
            body: "".to_string(),
            source: "webhook".to_string(),
            source_id: "".to_string(),
            author: "user".to_string(),
            url: "".to_string(),
            labels: labels.clone(),
            parent_id: None,
        })
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.labels, labels);
}

#[tokio::test]
async fn task_with_parent_id() {
    let store = TaskStore::open_memory().await.unwrap();

    let parent_id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Parent".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    let child_id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Child".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    store
        .set_fields(child_id, &[("parent_id", serde_json::json!(parent_id))])
        .await
        .unwrap();

    let child = store.get(child_id).await.unwrap();
    assert_eq!(child.parent_id, Some(parent_id));
}

#[tokio::test]
async fn multiple_runs_per_task() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Multi-run".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // Attempt 1: agent run fails
    let r1 = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "claude -p ...",
            prompt: "prompt v1",
        })
        .await
        .unwrap();
    store
        .complete_run(&CompleteRun {
            run_id: r1,
            exit_code: Some(1),
            stdout: "error output",
            stderr: "compile failed",
            parsed: "",
            outcome: "failed",
            error: "compilation error",
            tokens: RunTokenUsage {
                input_tokens: 30000,
                output_tokens: 5000,
                total_cost_usd: 0.10,
                duration_secs: 20.0,
            },
        })
        .await
        .unwrap();

    // Attempt 2: agent run succeeds
    let r2 = store
        .start_run(&StartRun {
            task_id,
            attempt: 2,
            run_type: "agent",
            agent: "claude",
            model: "opus",
            command: "claude -p ...",
            prompt: "prompt v2",
        })
        .await
        .unwrap();
    store
        .complete_run(&CompleteRun {
            run_id: r2,
            exit_code: Some(0),
            stdout: "success output",
            stderr: "",
            parsed: r#"{"summary":"done"}"#,
            outcome: "success",
            error: "",
            tokens: RunTokenUsage {
                input_tokens: 40000,
                output_tokens: 8000,
                total_cost_usd: 0.80,
                duration_secs: 60.0,
            },
        })
        .await
        .unwrap();

    // Attempt 2: review run
    let r3 = store
        .start_run(&StartRun {
            task_id,
            attempt: 2,
            run_type: "review",
            agent: "claude",
            model: "sonnet",
            command: "claude -p review ...",
            prompt: "review prompt",
        })
        .await
        .unwrap();
    store
        .complete_run(&CompleteRun {
            run_id: r3,
            exit_code: Some(0),
            stdout: "LGTM",
            stderr: "",
            parsed: r#"{"verdict":"approve"}"#,
            outcome: "success",
            error: "",
            tokens: RunTokenUsage::default(),
        })
        .await
        .unwrap();

    // All 3 runs
    let runs = store.get_runs(task_id).await.unwrap();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0].attempt, 1);
    assert_eq!(runs[0].outcome, "failed");
    assert_eq!(runs[1].attempt, 2);
    assert_eq!(runs[1].run_type, "agent");
    assert_eq!(runs[2].attempt, 2);
    assert_eq!(runs[2].run_type, "review");

    // Last agent run is attempt 2
    let last_agent = store.get_last_run(task_id, "agent").await.unwrap().unwrap();
    assert_eq!(last_agent.attempt, 2);
    assert_eq!(last_agent.model, "opus");

    // Last review run
    let last_review = store
        .get_last_run(task_id, "review")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last_review.outcome, "success");
}

#[tokio::test]
async fn start_run_upserts_on_conflict() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // Start same run twice (same task_id, attempt, run_type)
    let id1 = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "cmd1",
            prompt: "prompt1",
        })
        .await
        .unwrap();

    let id2 = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "codex",
            model: "gpt-5.2",
            command: "cmd2",
            prompt: "prompt2",
        })
        .await
        .unwrap();

    assert_eq!(id1, id2, "upsert should return same ID");

    // Should have updated agent/model
    let runs = store.get_runs(task_id).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].agent, "codex");
    assert_eq!(runs[0].model, "gpt-5.2");
}

#[tokio::test]
async fn reset_counters_preserves_other_fields() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // Set some counters and non-counter fields
    store
        .set_fields(
            id,
            &[
                ("agent", serde_json::json!("claude")),
                ("branch", serde_json::json!("fix-123")),
                ("summary", serde_json::json!("did something")),
            ],
        )
        .await
        .unwrap();
    store.increment(id, "attempts").await.unwrap();
    store.increment(id, "attempts").await.unwrap();
    store.increment(id, "review_cycles").await.unwrap();
    store.increment(id, "network_retries").await.unwrap();

    // Reset
    store.reset_counters(id).await.unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.attempts, 0);
    assert_eq!(task.review_cycles, 0);
    assert_eq!(task.network_retries, 0);
    // Non-counter fields preserved
    assert_eq!(task.agent, Some("claude".to_string()));
    assert_eq!(task.branch, "fix-123");
    assert_eq!(task.summary, "did something");
}

#[tokio::test]
async fn reset_failure_counters_preserves_review_cycles() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // Simulate counters after a review cycle: review_cycles=1, ci_merge_failures=2, plus transient failures
    store.increment(id, "review_cycles").await.unwrap();
    store.increment(id, "ci_merge_failures").await.unwrap();
    store.increment(id, "ci_merge_failures").await.unwrap();
    store.increment(id, "attempts").await.unwrap();
    store.increment(id, "review_agent_failures").await.unwrap();
    store.increment(id, "merge_conflict_retries").await.unwrap();
    store.increment(id, "network_retries").await.unwrap();

    // reset_failure_counters must zero transient counters but preserve review_cycles, ci_merge_failures, and attempts
    store.reset_failure_counters(id).await.unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.review_cycles, 1, "review_cycles must be preserved");
    assert_eq!(
        task.ci_merge_failures, 2,
        "ci_merge_failures must be preserved"
    );
    assert_eq!(task.attempts, 1, "attempts must be preserved (monotonic)");
    assert_eq!(task.review_agent_failures, 0);
    assert_eq!(task.merge_conflict_retries, 0);
    assert_eq!(task.network_retries, 0);
}

#[tokio::test]
async fn status_lifecycle_full_flow() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: Some("42".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Full lifecycle".to_string(),
            body: "".to_string(),
            source: "webhook".to_string(),
            source_id: "".to_string(),
            author: "user".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // Walk through the full lifecycle
    let transitions = [
        TaskStatus::Routed,
        TaskStatus::InProgress,
        TaskStatus::NeedsReview,
        TaskStatus::InReview,
        TaskStatus::Done,
    ];

    for status in transitions {
        store.update_status(id, status).await.unwrap();
        let task = store.get(id).await.unwrap();
        assert_eq!(task.status, status);
    }
}

#[tokio::test]
async fn list_routable_returns_only_new() {
    let store = TaskStore::open_memory().await.unwrap();

    let id1 = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "New task".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    let _id2 = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Done task".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();
    store.update_status(_id2, TaskStatus::Done).await.unwrap();

    let routable = store.list_routable("owner/repo").await.unwrap();
    assert_eq!(routable.len(), 1);
    assert_eq!(routable[0].id, id1);
}

#[tokio::test]
async fn cleanable_excludes_in_progress_tasks() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "In progress".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    store
        .set_fields(id, &[("worktree", serde_json::json!("/tmp/wt"))])
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::InProgress)
        .await
        .unwrap();

    // In-progress task with worktree should NOT be cleanable
    let cleanable = store.list_cleanable("owner/repo").await.unwrap();
    assert_eq!(cleanable.len(), 0);

    // But blocked task with worktree SHOULD be cleanable
    store.update_status(id, TaskStatus::Blocked).await.unwrap();
    let cleanable = store.list_cleanable("owner/repo").await.unwrap();
    assert_eq!(cleanable.len(), 1);
}

#[tokio::test]
async fn delegations_stored_as_json() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    let delegations = serde_json::json!([
        {"task_id": 2, "reason": "sub-task"},
        {"task_id": 3, "reason": "follow-up"}
    ]);

    store
        .set_fields(id, &[("delegations", delegations.clone())])
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.delegations.len(), 2);
}

#[tokio::test]
async fn created_at_and_updated_at_are_set() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert!(!task.created_at.is_empty());
    assert!(!task.updated_at.is_empty());
    assert!(task.created_at.ends_with('Z'), "should be UTC");
    assert!(task.created_at.contains('T'), "should be RFC3339");
}

#[tokio::test]
async fn concurrent_increments() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // Sequential increments should produce correct values
    for expected in 1..=10 {
        let val = store.increment(id, "attempts").await.unwrap();
        assert_eq!(val, expected);
    }

    let task = store.get(id).await.unwrap();
    assert_eq!(task.attempts, 10);
}

#[tokio::test]
async fn internal_task_has_no_external_id() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Internal".to_string(),
            body: "".to_string(),
            source: "cron".to_string(),
            source_id: "daily".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert!(task.external_id.is_none());
    assert_eq!(task.origin, "internal");
}

#[tokio::test]
async fn default_values_are_correct() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Defaults".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::New);
    assert_eq!(task.complexity, "medium");
    assert_eq!(task.attempts, 0);
    assert_eq!(task.route_attempts, 0);
    assert_eq!(task.merge_conflict_retries, 0);
    assert_eq!(task.ci_merge_failures, 0);
    assert_eq!(task.pr_create_failures, 0);
    assert_eq!(task.push_failures, 0);
    assert_eq!(task.review_agent_failures, 0);
    assert_eq!(task.review_cycles, 0);
    assert_eq!(task.input_tokens, 0);
    assert_eq!(task.output_tokens, 0);
    assert!((task.total_cost_usd - 0.0).abs() < f64::EPSILON);
    assert!(!task.worktree_cleaned);
    assert!(!task.budget_exceeded);
    assert!(task.memory.is_empty());
    assert!(task.delegations.is_empty());
    assert!(task.pr_number.is_none());
    assert!(task.parent_id.is_none());
    assert!(task.agent.is_none());
    assert!(task.model.is_none());
    assert!(task.block_reason.is_none());
}

#[tokio::test]
async fn list_all_returns_all_tasks_in_repo() {
    let store = TaskStore::open_memory().await.unwrap();

    // Create tasks in two repos
    store
        .create(&NewTask {
            external_id: Some("1".to_string()),
            repo: "owner/repo-a".to_string(),
            origin: "github".to_string(),
            title: "Task A1".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .create(&NewTask {
            external_id: Some("2".to_string()),
            repo: "owner/repo-a".to_string(),
            origin: "github".to_string(),
            title: "Task A2".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .create(&NewTask {
            external_id: Some("3".to_string()),
            repo: "owner/repo-b".to_string(),
            origin: "github".to_string(),
            title: "Task B1".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let all_a = store.list_all("owner/repo-a").await.unwrap();
    assert_eq!(all_a.len(), 2);

    let all_b = store.list_all("owner/repo-b").await.unwrap();
    assert_eq!(all_b.len(), 1);
    assert_eq!(all_b[0].title, "Task B1");
}

#[tokio::test]
async fn list_all_active_global_returns_tasks_across_repos() {
    let store = TaskStore::open_memory().await.unwrap();

    // Tasks in two different repos
    let id1 = store
        .create(&NewTask {
            external_id: Some("1".to_string()),
            repo: "owner/repo-a".to_string(),
            origin: "github".to_string(),
            title: "Active A".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .create(&NewTask {
            external_id: Some("2".to_string()),
            repo: "owner/repo-b".to_string(),
            origin: "github".to_string(),
            title: "Active B".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    // Mark one as done — should be excluded
    store.update_status(id1, TaskStatus::Done).await.unwrap();

    let active = store.list_all_active_global().await.unwrap();
    assert_eq!(active.len(), 1, "only non-done tasks returned: {active:?}");
    assert_eq!(active[0].title, "Active B");
}

#[tokio::test]
async fn list_all_by_status_global_filters_correctly() {
    let store = TaskStore::open_memory().await.unwrap();

    let id1 = store
        .create(&NewTask {
            external_id: Some("1".to_string()),
            repo: "owner/repo-a".to_string(),
            origin: "github".to_string(),
            title: "New task".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .create(&NewTask {
            external_id: Some("2".to_string()),
            repo: "owner/repo-b".to_string(),
            origin: "github".to_string(),
            title: "Another new task".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .update_status(id1, TaskStatus::InProgress)
        .await
        .unwrap();

    let new_tasks = store
        .list_all_by_status_global(TaskStatus::New)
        .await
        .unwrap();
    assert_eq!(
        new_tasks.len(),
        1,
        "one new task across repos: {new_tasks:?}"
    );
    assert_eq!(new_tasks[0].title, "Another new task");

    let in_progress = store
        .list_all_by_status_global(TaskStatus::InProgress)
        .await
        .unwrap();
    assert_eq!(in_progress.len(), 1);
    assert_eq!(in_progress[0].title, "New task");
}

#[tokio::test]
async fn cost_summary_aggregates_correctly() {
    let store = TaskStore::open_memory().await.unwrap();

    let id1 = store
        .create(&NewTask {
            external_id: Some("1".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Task 1".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let id2 = store
        .create(&NewTask {
            external_id: Some("2".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Task 2".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    store.store_tokens(id1, 1000, 500, "sonnet").await.unwrap();
    store.store_tokens(id2, 2000, 1000, "opus").await.unwrap();

    let (input, output, cost) = store.cost_summary("owner/repo").await.unwrap();
    assert_eq!(input, 3000);
    assert_eq!(output, 1500);
    assert!(cost > 0.0);
}

#[tokio::test]
async fn status_counts_groups_correctly() {
    let store = TaskStore::open_memory().await.unwrap();

    for i in 0..3 {
        store
            .create(&NewTask {
                external_id: Some(format!("{}", i)),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: format!("Task {}", i),
                ..Default::default()
            })
            .await
            .unwrap();
    }

    // Move task 1 to routed, task 2 to done
    store.update_status(2, TaskStatus::Routed).await.unwrap();
    store.update_status(3, TaskStatus::Done).await.unwrap();

    let counts = store.status_counts("owner/repo").await.unwrap();
    assert_eq!(counts.get("new"), Some(&1));
    assert_eq!(counts.get("routed"), Some(&1));
    assert_eq!(counts.get("done"), Some(&1));
}

#[tokio::test]
async fn ensure_external_task_upserts() {
    let store = TaskStore::open_memory().await.unwrap();

    let ext = crate::backends::ExternalTask {
        id: crate::backends::ExternalId("42".to_string()),
        title: "Test issue".to_string(),
        body: "Body text".to_string(),
        state: "open".to_string(),
        labels: vec!["bug".to_string()],
        author: "user".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: "https://github.com/owner/repo/issues/42".to_string(),
    };

    let id1 = store
        .ensure_external_task("owner/repo", &ext)
        .await
        .unwrap();
    let id2 = store
        .ensure_external_task("owner/repo", &ext)
        .await
        .unwrap();
    assert_eq!(id1, id2, "should upsert, not create duplicate");

    let task = store.get(id1).await.unwrap();
    assert_eq!(task.external_id, Some("42".to_string()));
    assert_eq!(task.title, "Test issue");
}

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
async fn set_fields_persists_review_session_expected() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: Some("42".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Issue 42".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    store
        .set_fields(id, &[("review_session_expected", serde_json::json!(true))])
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert!(task.review_session_expected);
}

// ── prune_old_runs ──────────────────────────────────────────────────────

#[tokio::test]
async fn prune_old_runs_deletes_runs_for_old_done_tasks() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Old task".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    // Add a run
    let run_id = store
        .start_run(&StartRun {
            task_id: id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "echo test",
            prompt: "do stuff",
        })
        .await
        .unwrap();
    store
        .complete_run(&CompleteRun {
            run_id,
            exit_code: Some(0),
            stdout: "ok",
            stderr: "",
            parsed: "{}",
            outcome: "success",
            error: "",
            tokens: RunTokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                total_cost_usd: 0.001,
                duration_secs: 5.0,
            },
        })
        .await
        .unwrap();

    // Mark done and backdate updated_at
    store.update_status(id, TaskStatus::Done).await.unwrap();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(&store.pool)
        .await
        .unwrap();

    // Prune runs older than 30 days
    let pruned = store.prune_old_runs(30).await.unwrap();
    assert_eq!(pruned, 1, "should prune 1 run for old done task");

    // Verify runs are gone
    let runs = store.get_runs(id).await.unwrap();
    assert!(runs.is_empty(), "runs should be deleted after prune");
}

#[tokio::test]
async fn prune_old_runs_keeps_runs_for_active_tasks() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Active task".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    store
        .start_run(&StartRun {
            task_id: id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "",
            prompt: "",
        })
        .await
        .unwrap();

    // Task stays in 'new' status — not done/blocked
    // Backdate it too
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(&store.pool)
        .await
        .unwrap();

    let pruned = store.prune_old_runs(30).await.unwrap();
    assert_eq!(
        pruned, 0,
        "should not prune runs for active (non-done/blocked) tasks"
    );

    let runs = store.get_runs(id).await.unwrap();
    assert_eq!(runs.len(), 1, "run should still exist");
}

#[tokio::test]
async fn prune_old_runs_keeps_recent_done_tasks() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Recent done task".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    store
        .start_run(&StartRun {
            task_id: id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "",
            prompt: "",
        })
        .await
        .unwrap();

    // Mark done but keep updated_at recent (default is now)
    store.update_status(id, TaskStatus::Done).await.unwrap();

    let pruned = store.prune_old_runs(30).await.unwrap();
    assert_eq!(
        pruned, 0,
        "should not prune runs for recently completed tasks"
    );
}

// ── complete_run with full lifecycle ─────────────────────────────────────

#[tokio::test]
async fn complete_run_records_all_fields() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let run_id = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "opus",
            command: "claude --model opus",
            prompt: "Fix the bug",
        })
        .await
        .unwrap();

    store
        .complete_run(&CompleteRun {
            run_id,
            exit_code: Some(0),
            stdout: "Fixed it!",
            stderr: "warning: unused var",
            parsed: r#"{"summary":"Fixed bug"}"#,
            outcome: "success",
            error: "",
            tokens: RunTokenUsage {
                input_tokens: 5000,
                output_tokens: 2000,
                total_cost_usd: 0.105,
                duration_secs: 45.3,
            },
        })
        .await
        .unwrap();

    let run = store.get_last_run(task_id, "agent").await.unwrap().unwrap();
    assert_eq!(run.agent, "claude");
    assert_eq!(run.model, "opus");
    assert_eq!(run.command, "claude --model opus");
    assert_eq!(run.prompt, "Fix the bug");
    assert_eq!(run.exit_code, Some(0));
    assert_eq!(run.stdout, "Fixed it!");
    assert_eq!(run.stderr, "warning: unused var");
    assert_eq!(run.parsed_response, r#"{"summary":"Fixed bug"}"#);
    assert_eq!(run.outcome, "success");
    assert!(run.error.is_empty());
    assert_eq!(run.input_tokens, 5000);
    assert_eq!(run.output_tokens, 2000);
    assert!((run.total_cost_usd - 0.105).abs() < 0.001);
    assert!((run.duration_secs - 45.3).abs() < 0.1);
    assert!(run.completed_at.is_some());
}

#[tokio::test]
async fn get_last_run_filters_by_run_type() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    // Create runs of different types
    store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "route",
            agent: "claude",
            model: "haiku",
            command: "",
            prompt: "route this",
        })
        .await
        .unwrap();
    store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "codex",
            model: "gpt-5.2",
            command: "codex run",
            prompt: "fix bug",
        })
        .await
        .unwrap();
    store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "review",
            agent: "claude",
            model: "sonnet",
            command: "",
            prompt: "review PR",
        })
        .await
        .unwrap();

    let route_run = store.get_last_run(task_id, "route").await.unwrap().unwrap();
    assert_eq!(route_run.agent, "claude");
    assert_eq!(route_run.model, "haiku");

    let agent_run = store.get_last_run(task_id, "agent").await.unwrap().unwrap();
    assert_eq!(agent_run.agent, "codex");

    let review_run = store
        .get_last_run(task_id, "review")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(review_run.prompt, "review PR");

    let missing = store.get_last_run(task_id, "nonexistent").await.unwrap();
    assert!(missing.is_none());
}

#[tokio::test]
async fn get_runs_returns_all_attempts_ordered() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Multi-attempt".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    for attempt in 1..=3 {
        store
            .start_run(&StartRun {
                task_id,
                attempt,
                run_type: "agent",
                agent: "claude",
                model: "sonnet",
                command: "",
                prompt: &format!("attempt {attempt}"),
            })
            .await
            .unwrap();
    }

    let runs = store.get_runs(task_id).await.unwrap();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0].attempt, 1);
    assert_eq!(runs[1].attempt, 2);
    assert_eq!(runs[2].attempt, 3);
    assert_eq!(runs[0].prompt, "attempt 1");
    assert_eq!(runs[2].prompt, "attempt 3");
}

// ── cost_summary edge cases ─────────────────────────────────────────────

#[tokio::test]
async fn cost_summary_returns_zeros_for_empty_repo() {
    let store = TaskStore::open_memory().await.unwrap();
    let (input, output, cost) = store.cost_summary("empty/repo").await.unwrap();
    assert_eq!(input, 0);
    assert_eq!(output, 0);
    assert!((cost - 0.0).abs() < f64::EPSILON);
}

// ── status_counts edge cases ────────────────────────────────────────────

#[tokio::test]
async fn status_counts_returns_empty_map_for_empty_repo() {
    let store = TaskStore::open_memory().await.unwrap();
    let counts = store.status_counts("empty/repo").await.unwrap();
    assert!(counts.is_empty());
}

// ── list_all with mixed statuses ────────────────────────────────────────

#[tokio::test]
async fn list_all_includes_all_statuses() {
    let store = TaskStore::open_memory().await.unwrap();

    let _id1 = store
        .create(&NewTask {
            external_id: Some("1".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "New task".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let id2 = store
        .create(&NewTask {
            external_id: Some("2".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Done task".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let id3 = store
        .create(&NewTask {
            external_id: Some("3".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Blocked task".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    store.update_status(id2, TaskStatus::Done).await.unwrap();
    store.update_status(id3, TaskStatus::Blocked).await.unwrap();

    let all = store.list_all("owner/repo").await.unwrap();
    assert_eq!(
        all.len(),
        3,
        "list_all should include new, done, and blocked"
    );

    // Verify statuses
    let statuses: Vec<_> = all.iter().map(|t| t.status).collect();
    assert!(statuses.contains(&TaskStatus::New));
    assert!(statuses.contains(&TaskStatus::Done));
    assert!(statuses.contains(&TaskStatus::Blocked));
}

// ── ensure_external_task updates title on re-upsert ─────────────────────

#[tokio::test]
async fn ensure_external_task_updates_on_reupsert() {
    let store = TaskStore::open_memory().await.unwrap();

    let ext1 = crate::backends::ExternalTask {
        id: crate::backends::ExternalId("42".to_string()),
        title: "Original title".to_string(),
        body: "Original body".to_string(),
        state: "open".to_string(),
        labels: vec!["bug".to_string()],
        author: "user".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: "https://github.com/owner/repo/issues/42".to_string(),
    };

    let id1 = store
        .ensure_external_task("owner/repo", &ext1)
        .await
        .unwrap();

    // Update the title
    let ext2 = crate::backends::ExternalTask {
        title: "Updated title".to_string(),
        body: "Updated body".to_string(),
        ..ext1
    };

    let id2 = store
        .ensure_external_task("owner/repo", &ext2)
        .await
        .unwrap();
    assert_eq!(id1, id2);

    let task = store.get(id1).await.unwrap();
    assert_eq!(task.title, "Updated title");
    assert_eq!(task.body, "Updated body");
}

// ── store_route + list verifies routing data persists ───────────────────

#[tokio::test]
async fn store_route_persists_all_routing_fields() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: Some("99".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Route me".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    store
        .store_route(&StoreRoute {
            id,
            agent: "codex",
            model: Some("gpt-5.2"),
            complexity: "complex",
            estimate: 13,
            reason: "needs deep refactoring",
            profile: r#"{"role":"backend"}"#,
            skills: r#"["git","rust"]"#,
        })
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.agent.as_deref(), Some("codex"));
    assert_eq!(task.model.as_deref(), Some("gpt-5.2"));
    assert_eq!(task.complexity, "complex");
    assert_eq!(task.estimate, 13);
    assert_eq!(task.route_reason, "needs deep refactoring");
    assert_eq!(task.agent_profile, r#"{"role":"backend"}"#);
    assert_eq!(task.selected_skills, r#"["git","rust"]"#);
}

// ── full run lifecycle: start → complete → query ────────────────────────

#[tokio::test]
async fn run_lifecycle_start_complete_query() {
    let store = TaskStore::open_memory().await.unwrap();

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Lifecycle test".to_string(),
            external_id: Some("100".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();

    // Attempt 1: agent run
    let run1 = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "claude --model sonnet",
            prompt: "Fix the login bug",
        })
        .await
        .unwrap();

    store
        .complete_run(&CompleteRun {
            run_id: run1,
            exit_code: Some(1),
            stdout: "Error: couldn't find file",
            stderr: "WARN: deprecated API",
            parsed: r#"{"status":"failed"}"#,
            outcome: "failed",
            error: "file not found",
            tokens: RunTokenUsage {
                input_tokens: 3000,
                output_tokens: 1500,
                total_cost_usd: 0.045,
                duration_secs: 30.0,
            },
        })
        .await
        .unwrap();

    // Attempt 2: retry with different model
    let run2 = store
        .start_run(&StartRun {
            task_id,
            attempt: 2,
            run_type: "agent",
            agent: "claude",
            model: "opus",
            command: "claude --model opus",
            prompt: "Fix the login bug (retry)",
        })
        .await
        .unwrap();

    store
        .complete_run(&CompleteRun {
            run_id: run2,
            exit_code: Some(0),
            stdout: "Fixed the bug by updating the path",
            stderr: "",
            parsed: r#"{"status":"done","summary":"Fixed path"}"#,
            outcome: "success",
            error: "",
            tokens: RunTokenUsage {
                input_tokens: 5000,
                output_tokens: 3000,
                total_cost_usd: 0.12,
                duration_secs: 60.0,
            },
        })
        .await
        .unwrap();

    // Attempt 1: review run
    let run3 = store
        .start_run(&StartRun {
            task_id,
            attempt: 1,
            run_type: "review",
            agent: "codex",
            model: "gpt-5.2",
            command: "codex review",
            prompt: "Review PR #42",
        })
        .await
        .unwrap();

    store
        .complete_run(&CompleteRun {
            run_id: run3,
            exit_code: Some(0),
            stdout: "LGTM",
            stderr: "",
            parsed: r#"{"decision":"approve"}"#,
            outcome: "success",
            error: "",
            tokens: RunTokenUsage {
                input_tokens: 2000,
                output_tokens: 500,
                total_cost_usd: 0.03,
                duration_secs: 15.0,
            },
        })
        .await
        .unwrap();

    // Query: get all runs
    let all_runs = store.get_runs(task_id).await.unwrap();
    assert_eq!(all_runs.len(), 3);
    assert_eq!(all_runs[0].attempt, 1);
    assert_eq!(all_runs[0].run_type, "agent");
    assert_eq!(all_runs[0].outcome, "failed");
    assert_eq!(all_runs[1].attempt, 1);
    assert_eq!(all_runs[1].run_type, "review");
    assert_eq!(all_runs[2].attempt, 2);
    assert_eq!(all_runs[2].run_type, "agent");
    assert_eq!(all_runs[2].outcome, "success");

    // Query: last agent run should be attempt 2
    let last_agent = store.get_last_run(task_id, "agent").await.unwrap().unwrap();
    assert_eq!(last_agent.attempt, 2);
    assert_eq!(last_agent.model, "opus");
    assert_eq!(last_agent.exit_code, Some(0));

    // Query: last review run
    let last_review = store
        .get_last_run(task_id, "review")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last_review.agent, "codex");
    assert_eq!(last_review.parsed_response, r#"{"decision":"approve"}"#);
}

// ── set_fields rejects unknown columns ──────────────────────────────────

#[tokio::test]
async fn set_fields_rejects_sql_injection_attempt() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    // Attempt to set a column not in the allowlist
    let result = store
        .set_fields(id, &[("id", serde_json::json!(999))])
        .await;
    assert!(result.is_err(), "should reject 'id' column");

    let result = store
        .set_fields(
            id,
            &[("'; DROP TABLE tasks; --", serde_json::json!("pwned"))],
        )
        .await;
    assert!(result.is_err(), "should reject SQL injection attempt");

    // Verify task is intact
    let task = store.get(id).await.unwrap();
    assert_eq!(task.id, id, "task should not be modified");
}

// ── append_memory + recent_memory ───────────────────────────────────────

#[tokio::test]
async fn memory_append_and_retrieve() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Memory test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    // Append memory entries
    let entry1 = MemoryEntry {
        attempt: 1,
        agent: "claude".to_string(),
        model: None,
        learnings: vec!["file is at src/main.rs".to_string()],
        error: None,
        files_modified: vec![],
        approach: "read the code".to_string(),
        timestamp: "2026-01-01T00:00:00Z".to_string(),
    };
    let entry2 = MemoryEntry {
        attempt: 2,
        agent: "codex".to_string(),
        model: None,
        learnings: vec!["need to update Cargo.toml too".to_string()],
        error: None,
        files_modified: vec![],
        approach: "edit files".to_string(),
        timestamp: "2026-01-01T01:00:00Z".to_string(),
    };

    store.append_memory(id, &entry1).await.unwrap();
    store.append_memory(id, &entry2).await.unwrap();

    // Retrieve recent
    let recent = store.recent_memory(id, 10).await.unwrap();
    assert_eq!(recent.len(), 2);

    // Retrieve with limit
    let limited = store.recent_memory(id, 1).await.unwrap();
    assert_eq!(limited.len(), 1);
}

// ── mark_cleaned + list_cleanable ───────────────────────────────────────

#[tokio::test]
async fn mark_cleaned_and_list_cleanable() {
    let store = TaskStore::open_memory().await.unwrap();

    let id1 = store
        .create(&NewTask {
            external_id: Some("1".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Task 1".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let id2 = store
        .create(&NewTask {
            external_id: Some("2".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Task 2".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    // Set worktrees and mark done
    store
        .set_fields(id1, &[("worktree", serde_json::json!("/tmp/wt1"))])
        .await
        .unwrap();
    store
        .set_fields(id2, &[("worktree", serde_json::json!("/tmp/wt2"))])
        .await
        .unwrap();
    store.update_status(id1, TaskStatus::Done).await.unwrap();
    store.update_status(id2, TaskStatus::Done).await.unwrap();

    // Both should be cleanable
    let cleanable = store.list_cleanable("owner/repo").await.unwrap();
    assert_eq!(cleanable.len(), 2);

    // Mark one as cleaned
    store.mark_cleaned(id1).await.unwrap();

    let cleanable = store.list_cleanable("owner/repo").await.unwrap();
    assert_eq!(cleanable.len(), 1);
    assert_eq!(cleanable[0].id, id2);

    // Verify the flag persisted
    let task = store.get(id1).await.unwrap();
    assert!(task.worktree_cleaned);
}

// ── store_tokens accumulates ────────────────────────────────────────────

#[tokio::test]
async fn store_tokens_accumulates_across_calls() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Token test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    store.store_tokens(id, 1000, 500, "sonnet").await.unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(task.input_tokens, 1000);
    assert_eq!(task.output_tokens, 500);
    let cost1 = task.total_cost_usd;
    assert!(cost1 > 0.0);

    // Second call accumulates (not replaces)
    store.store_tokens(id, 2000, 1000, "sonnet").await.unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(task.input_tokens, 3000);
    assert_eq!(task.output_tokens, 1500);
    assert!(task.total_cost_usd > cost1);
}

// ── increment field ─────────────────────────────────────────────────────

#[tokio::test]
async fn increment_increases_counter() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Increment test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let v1 = store.increment(id, "attempts").await.unwrap();
    assert_eq!(v1, 1);
    let v2 = store.increment(id, "attempts").await.unwrap();
    assert_eq!(v2, 2);
    let v3 = store.increment(id, "review_cycles").await.unwrap();
    assert_eq!(v3, 1);

    let task = store.get(id).await.unwrap();
    assert_eq!(task.attempts, 2);
    assert_eq!(task.review_cycles, 1);
}

// ── with_store on TaskRunner ─────────────────────────────────────────

#[tokio::test]
async fn task_runner_with_store() {
    use crate::engine::runner::TaskRunner;
    use std::sync::Arc;

    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    // Just verify it compiles and the builder works
    let _runner = TaskRunner::new("owner/repo".to_string()).with_store(store);
}

#[tokio::test]
async fn append_memory_within_transaction() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Memory tx test".to_string(),
            body: "".to_string(),
            source: "".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // Append two entries sequentially — both should be preserved
    store
        .append_memory(
            id,
            &MemoryEntry {
                attempt: 1,
                agent: "claude".to_string(),
                model: Some("opus".to_string()),
                learnings: vec!["first learning".to_string()],
                error: None,
                files_modified: vec![],
                approach: "first approach".to_string(),
                timestamp: String::new(),
            },
        )
        .await
        .unwrap();
    store
        .append_memory(
            id,
            &MemoryEntry {
                attempt: 2,
                agent: "codex".to_string(),
                model: Some("gpt-5".to_string()),
                learnings: vec!["second learning".to_string()],
                error: None,
                files_modified: vec![],
                approach: "second approach".to_string(),
                timestamp: String::new(),
            },
        )
        .await
        .unwrap();

    let mem = store.recent_memory(id, 10).await.unwrap();
    assert_eq!(mem.len(), 2);
    assert_eq!(mem[0].approach, "first approach");
    assert_eq!(mem[1].approach, "second approach");
}

#[tokio::test]
async fn set_fields_with_numeric_bool_null_types() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: Some("99".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Type test".to_string(),
            body: "".to_string(),
            source: "".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // Set a numeric field
    store
        .set_fields(id, &[("pr_number", serde_json::json!(42))])
        .await
        .unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(task.pr_number, Some(42));

    // Set a boolean field
    store
        .set_fields(id, &[("budget_exceeded", serde_json::json!(true))])
        .await
        .unwrap();
    let task = store.get(id).await.unwrap();
    assert!(task.budget_exceeded);

    // Set a field to null (empty string)
    store
        .set_fields(id, &[("summary", serde_json::Value::Null)])
        .await
        .unwrap();
    let task = store.get(id).await.unwrap();
    assert!(task.summary.is_empty());
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
            body: "".to_string(),
            source: "".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
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
async fn list_cleanable_excludes_active_and_already_cleaned() {
    let store = TaskStore::open_memory().await.unwrap();

    // Create tasks in various states
    let done_id = store
        .create(&NewTask {
            external_id: Some("1".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Done task".to_string(),
            body: "".to_string(),
            source: "".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();
    store
        .update_status(done_id, TaskStatus::Done)
        .await
        .unwrap();
    store
        .set_fields(done_id, &[("worktree", serde_json::json!("/tmp/wt1"))])
        .await
        .unwrap();

    let active_id = store
        .create(&NewTask {
            external_id: Some("2".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Active task".to_string(),
            body: "".to_string(),
            source: "".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();
    store
        .update_status(active_id, TaskStatus::InProgress)
        .await
        .unwrap();
    store
        .set_fields(active_id, &[("worktree", serde_json::json!("/tmp/wt2"))])
        .await
        .unwrap();

    let cleaned_id = store
        .create(&NewTask {
            external_id: Some("3".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Already cleaned".to_string(),
            body: "".to_string(),
            source: "".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();
    store
        .update_status(cleaned_id, TaskStatus::Done)
        .await
        .unwrap();
    store
        .set_fields(cleaned_id, &[("worktree", serde_json::json!("/tmp/wt3"))])
        .await
        .unwrap();
    store.mark_cleaned(cleaned_id).await.unwrap();

    let cleanable = store.list_cleanable("owner/repo").await.unwrap();
    assert_eq!(cleanable.len(), 1);
    assert_eq!(cleanable[0].id, done_id);
}

#[tokio::test]
async fn prune_old_runs_only_removes_old_terminal_tasks() {
    let store = TaskStore::open_memory().await.unwrap();

    // Create a done task
    let done_id = store
        .create(&NewTask {
            external_id: Some("1".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Old done".to_string(),
            body: "".to_string(),
            source: "".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();
    store
        .update_status(done_id, TaskStatus::Done)
        .await
        .unwrap();

    // Backdate the updated_at to 60 days ago
    sqlx::query("UPDATE tasks SET updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-60 days') WHERE id = ?")
        .bind(done_id)
        .execute(store.pool())
        .await
        .unwrap();

    // Add a run to the old done task
    store
        .start_run(&StartRun {
            task_id: done_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "opus",
            command: "cmd",
            prompt: "prompt",
        })
        .await
        .unwrap();

    // Create an active task with a run
    let active_id = store
        .create(&NewTask {
            external_id: Some("2".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Active".to_string(),
            body: "".to_string(),
            source: "".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();
    store
        .start_run(&StartRun {
            task_id: active_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "opus",
            command: "cmd",
            prompt: "prompt",
        })
        .await
        .unwrap();

    // Prune runs older than 30 days
    let pruned = store.prune_old_runs(30).await.unwrap();
    assert_eq!(pruned, 1);

    // Old done task's runs are gone
    let old_runs = store.get_runs(done_id).await.unwrap();
    assert!(old_runs.is_empty());

    // Active task's runs are preserved
    let active_runs = store.get_runs(active_id).await.unwrap();
    assert_eq!(active_runs.len(), 1);
}

#[tokio::test]
async fn foreign_key_constraint_on_task_runs() {
    let store = TaskStore::open_memory().await.unwrap();

    // Try to create a run for a non-existent task — should fail with FK constraint
    let result = store
        .start_run(&StartRun {
            task_id: 99999,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "opus",
            command: "cmd",
            prompt: "prompt",
        })
        .await;
    assert!(
        result.is_err(),
        "FK constraint should prevent orphaned runs"
    );
}

#[tokio::test]
async fn set_fields_multiple_fields_at_once() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: Some("1".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Multi-field".to_string(),
            body: "".to_string(),
            source: "".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // Set multiple fields atomically
    store
        .set_fields(
            id,
            &[
                ("agent", serde_json::json!("claude")),
                ("model", serde_json::json!("opus")),
                ("complexity", serde_json::json!("complex")),
                ("branch", serde_json::json!("feat/test")),
                ("pr_number", serde_json::json!(42)),
                ("attempts", serde_json::json!(3)),
            ],
        )
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.agent, Some("claude".to_string()));
    assert_eq!(task.model, Some("opus".to_string()));
    assert_eq!(task.complexity, "complex");
    assert_eq!(task.branch, "feat/test");
    assert_eq!(task.pr_number, Some(42));
    assert_eq!(task.attempts, 3);
}

#[tokio::test]
async fn store_tokens_overwrites_previous_values() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Token test".to_string(),
            body: "".to_string(),
            source: "".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // First token store
    store.store_tokens(id, 1000, 500, "haiku").await.unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(task.input_tokens, 1000);
    assert_eq!(task.output_tokens, 500);

    // Second store accumulates (not overwrites)
    store.store_tokens(id, 2000, 1000, "sonnet").await.unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(task.input_tokens, 3000);
    assert_eq!(task.output_tokens, 1500);
    assert_eq!(task.model, Some("sonnet".to_string()));
}

#[tokio::test]
async fn cost_summary_across_multiple_tasks() {
    let store = TaskStore::open_memory().await.unwrap();

    for i in 1..=3 {
        let id = store
            .create(&NewTask {
                external_id: Some(i.to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: format!("Task {i}"),
                body: "".to_string(),
                source: "".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
                parent_id: None,
            })
            .await
            .unwrap();
        store
            .store_tokens(id, 1000 * i, 500 * i, "haiku")
            .await
            .unwrap();
    }

    let (input, output, _cost) = store.cost_summary("owner/repo").await.unwrap();
    assert_eq!(input, 6000); // 1000 + 2000 + 3000
    assert_eq!(output, 3000); // 500 + 1000 + 1500
}

#[tokio::test]
async fn status_counts_with_mixed_statuses() {
    let store = TaskStore::open_memory().await.unwrap();

    for i in 1..=5 {
        let id = store
            .create(&NewTask {
                external_id: Some(i.to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: format!("Task {i}"),
                body: "".to_string(),
                source: "".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
                parent_id: None,
            })
            .await
            .unwrap();
        if i <= 2 {
            store
                .update_status(id, TaskStatus::InProgress)
                .await
                .unwrap();
        } else if i == 3 {
            store.update_status(id, TaskStatus::Done).await.unwrap();
        }
        // Tasks 4-5 stay New
    }

    let counts = store.status_counts("owner/repo").await.unwrap();
    assert_eq!(counts.get("new").copied().unwrap_or(0), 2);
    assert_eq!(counts.get("in_progress").copied().unwrap_or(0), 2);
    assert_eq!(counts.get("done").copied().unwrap_or(0), 1);
}

#[tokio::test]
async fn ensure_external_task_creates_then_updates() {
    use crate::backends::{ExternalId, ExternalTask};

    let store = TaskStore::open_memory().await.unwrap();

    let ext1 = ExternalTask {
        id: ExternalId("55".to_string()),
        title: "Original title".to_string(),
        body: "Original body".to_string(),
        state: "open".to_string(),
        labels: vec![],
        author: "user".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: "https://github.com/owner/repo/issues/55".to_string(),
    };
    let id1 = store
        .ensure_external_task("owner/repo", &ext1)
        .await
        .unwrap();

    let ext2 = ExternalTask {
        id: ExternalId("55".to_string()),
        title: "Updated title".to_string(),
        body: "Updated body".to_string(),
        state: "open".to_string(),
        labels: vec![],
        author: "user".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-02T00:00:00Z".to_string(),
        url: "https://github.com/owner/repo/issues/55".to_string(),
    };
    let id2 = store
        .ensure_external_task("owner/repo", &ext2)
        .await
        .unwrap();

    assert_eq!(id1, id2);
    let task = store.get(id1).await.unwrap();
    assert_eq!(task.title, "Updated title");
    assert_eq!(task.body, "Updated body");
}

// ── resolve_task_id: internal tasks ────────────────────────────────────

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
    assert_eq!(task.external_id, Some(format!("internal:{}", id)));
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
async fn list_internal_by_status_filters_origin() {
    let store = TaskStore::open_memory().await.unwrap();

    // Create one internal and one external task, both with status New
    store
        .create_internal("owner/repo", "Internal task", "", "cron", "j1", None)
        .await
        .unwrap();
    store
        .create(&NewTask {
            external_id: Some("42".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "External task".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let internal = store
        .list_internal_by_status("owner/repo", TaskStatus::New)
        .await
        .unwrap();
    assert_eq!(internal.len(), 1);
    assert_eq!(internal[0].title, "Internal task");
    assert_eq!(internal[0].origin, "internal");
}

#[tokio::test]
async fn list_all_internal_returns_only_internal() {
    let store = TaskStore::open_memory().await.unwrap();

    store
        .create_internal("owner/repo", "Int 1", "", "cron", "j1", None)
        .await
        .unwrap();
    store
        .create_internal("owner/repo", "Int 2", "", "manual", "", None)
        .await
        .unwrap();
    store
        .create(&NewTask {
            external_id: Some("99".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "External".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let all = store.list_all_internal("owner/repo").await.unwrap();
    assert_eq!(all.len(), 2);
    assert!(all.iter().all(|t| t.origin == "internal"));
}

// ── KV Store ─────────────────────────────────────────────────────

#[tokio::test]
async fn kv_get_missing_returns_none() {
    let store = TaskStore::open_memory().await.unwrap();
    assert_eq!(store.kv_get("missing").await.unwrap(), None);
}

#[tokio::test]
async fn kv_set_and_get() {
    let store = TaskStore::open_memory().await.unwrap();
    store.kv_set("foo", "bar").await.unwrap();
    assert_eq!(store.kv_get("foo").await.unwrap(), Some("bar".to_string()));
}

#[tokio::test]
async fn kv_set_upserts() {
    let store = TaskStore::open_memory().await.unwrap();
    store.kv_set("key", "v1").await.unwrap();
    store.kv_set("key", "v2").await.unwrap();
    assert_eq!(store.kv_get("key").await.unwrap(), Some("v2".to_string()));
}

#[tokio::test]
async fn kv_multiple_keys() {
    let store = TaskStore::open_memory().await.unwrap();
    store.kv_set("a", "1").await.unwrap();
    store.kv_set("b", "2").await.unwrap();
    assert_eq!(store.kv_get("a").await.unwrap(), Some("1".to_string()));
    assert_eq!(store.kv_get("b").await.unwrap(), Some("2".to_string()));
}

#[tokio::test]
async fn kv_increment_missing_starts_at_one() {
    let store = TaskStore::open_memory().await.unwrap();
    assert_eq!(store.kv_increment("counter").await.unwrap(), 1);
}

#[tokio::test]
async fn kv_increment_existing_adds_one() {
    let store = TaskStore::open_memory().await.unwrap();
    assert_eq!(store.kv_increment("counter").await.unwrap(), 1);
    assert_eq!(store.kv_increment("counter").await.unwrap(), 2);
    assert_eq!(store.kv_increment("counter").await.unwrap(), 3);
}

#[tokio::test]
async fn kv_increment_independent_keys() {
    let store = TaskStore::open_memory().await.unwrap();
    assert_eq!(store.kv_increment("a").await.unwrap(), 1);
    assert_eq!(store.kv_increment("b").await.unwrap(), 1);
    assert_eq!(store.kv_increment("a").await.unwrap(), 2);
    assert_eq!(store.kv_get("b").await.unwrap(), Some("1".to_string()));
}

// ── Task Metrics ─────────────────────────────────────────────────

#[tokio::test]
async fn insert_and_query_task_metric() {
    use chrono::Utc;
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();

    let metric = InsertTaskMetric {
        repo: "",
        task_id: "42",
        agent: "claude",
        model: Some("opus"),
        complexity: Some("complex"),
        outcome: "success",
        duration_seconds: 120.5,
        started_at: &now,
        completed_at: &now,
        attempts: 1,
        files_changed: 3,
        error_type: None,
        input_tokens: Some(5000),
        output_tokens: Some(2000),
        input_cost_usd: Some(0.05),
        output_cost_usd: Some(0.10),
        total_cost_usd: Some(0.15),
    };

    let id = store.insert_task_metric(&metric).await.unwrap();
    assert!(id > 0);
}

#[tokio::test]
async fn metrics_summary_empty() {
    let store = TaskStore::open_memory().await.unwrap();
    let summary = store.get_metrics_summary_24h().await.unwrap();
    assert_eq!(summary.tasks_completed_24h, 0);
    assert_eq!(summary.tasks_failed_24h, 0);
    assert!(summary.agent_stats.is_empty());
}

#[tokio::test]
async fn metrics_summary_counts_success_and_failure() {
    use chrono::Utc;
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();

    // Insert a success
    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "",
            task_id: "1",
            agent: "claude",
            model: None,
            complexity: Some("simple"),
            outcome: "success",
            duration_seconds: 60.0,
            started_at: &now,
            completed_at: &now,
            attempts: 1,
            files_changed: 1,
            error_type: None,
            input_tokens: None,
            output_tokens: None,
            input_cost_usd: None,
            output_cost_usd: None,
            total_cost_usd: None,
        })
        .await
        .unwrap();

    // Insert a failure
    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "",
            task_id: "2",
            agent: "codex",
            model: None,
            complexity: Some("medium"),
            outcome: "failed",
            duration_seconds: 30.0,
            started_at: &now,
            completed_at: &now,
            attempts: 2,
            files_changed: 0,
            error_type: Some("timeout"),
            input_tokens: None,
            output_tokens: None,
            input_cost_usd: None,
            output_cost_usd: None,
            total_cost_usd: None,
        })
        .await
        .unwrap();

    let summary = store.get_metrics_summary_24h().await.unwrap();
    assert_eq!(summary.tasks_completed_24h, 1);
    assert_eq!(summary.tasks_failed_24h, 1);
    assert_eq!(summary.agent_stats.len(), 2);
}

#[tokio::test]
async fn cost_summary_returns_correct_periods() {
    let store = TaskStore::open_memory().await.unwrap();
    let summary = store.get_cost_summary().await.unwrap();
    assert_eq!(summary.periods.len(), 3);
    assert_eq!(summary.periods[0].label, "24h");
    assert_eq!(summary.periods[1].label, "7d");
    assert_eq!(summary.periods[2].label, "30d");
}

#[tokio::test]
async fn cost_by_agent_empty() {
    let store = TaskStore::open_memory().await.unwrap();
    let groups = store.get_cost_by_agent().await.unwrap();
    assert!(groups.is_empty());
}

#[tokio::test]
async fn cost_by_model_empty() {
    let store = TaskStore::open_memory().await.unwrap();
    let groups = store.get_cost_by_model().await.unwrap();
    assert!(groups.is_empty());
}

#[tokio::test]
async fn cost_by_agent_groups_correctly() {
    use chrono::Utc;
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();

    for (agent, cost) in &[("claude", 0.10), ("codex", 0.05), ("claude", 0.20)] {
        store
            .insert_task_metric(&InsertTaskMetric {
                repo: "",
                task_id: "1",
                agent,
                model: None,
                complexity: None,
                outcome: "success",
                duration_seconds: 10.0,
                started_at: &now,
                completed_at: &now,
                attempts: 1,
                files_changed: 0,
                error_type: None,
                input_tokens: None,
                output_tokens: None,
                input_cost_usd: None,
                output_cost_usd: None,
                total_cost_usd: Some(*cost),
            })
            .await
            .unwrap();
    }

    let groups = store.get_cost_by_agent().await.unwrap();
    assert_eq!(groups.len(), 2);
    // claude should be first (higher total cost)
    assert_eq!(groups[0].name, "claude");
    assert_eq!(groups[0].task_count, 2);
    assert!((groups[0].total_cost_usd - 0.30).abs() < 0.001);
}

#[tokio::test]
async fn cost_by_model_groups_correctly() {
    use chrono::Utc;
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();

    for (model, cost) in &[("sonnet", 0.05), ("opus", 0.15), ("sonnet", 0.10)] {
        store
            .insert_task_metric(&InsertTaskMetric {
                repo: "",
                task_id: "1",
                agent: "claude",
                model: Some(model),
                complexity: None,
                outcome: "success",
                duration_seconds: 10.0,
                started_at: &now,
                completed_at: &now,
                attempts: 1,
                files_changed: 0,
                error_type: None,
                input_tokens: None,
                output_tokens: None,
                input_cost_usd: None,
                output_cost_usd: None,
                total_cost_usd: Some(*cost),
            })
            .await
            .unwrap();
    }

    let groups = store.get_cost_by_model().await.unwrap();
    assert_eq!(groups.len(), 2);
    // sonnet should be first (higher total cost: 0.15)
    assert_eq!(groups[0].name, "sonnet");
    assert_eq!(groups[0].task_count, 2);
    assert!((groups[0].total_cost_usd - 0.15).abs() < 0.001);
    assert_eq!(groups[1].name, "opus");
    assert_eq!(groups[1].task_count, 1);
}

// ── Rate Limits ─────────────────────────────────────────────────

#[tokio::test]
async fn record_rate_limit_returns_id() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .record_rate_limit("claude", "rate", Some("42"))
        .await
        .unwrap();
    assert!(id > 0);
}

#[tokio::test]
async fn rate_limits_counted_in_metrics_summary() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .record_rate_limit("claude", "rate", None)
        .await
        .unwrap();
    store
        .record_rate_limit("codex", "budget", Some("5"))
        .await
        .unwrap();

    let summary = store.get_metrics_summary_24h().await.unwrap();
    assert_eq!(summary.rate_limits_24h, 2);
}

// ── Self-improvement counter ─────────────────────────────────────

#[tokio::test]
async fn self_improvement_counter_starts_at_zero() {
    let store = TaskStore::open_memory().await.unwrap();
    let count = store.count_self_improvement_issues_7d().await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn self_improvement_counter_increments() {
    let store = TaskStore::open_memory().await.unwrap();
    store.increment_self_improvement_counter().await.unwrap();
    store.increment_self_improvement_counter().await.unwrap();
    let count = store.count_self_improvement_issues_7d().await.unwrap();
    assert_eq!(count, 2);
}

// ── Slow tasks & error distribution ─────────────────────────────

#[tokio::test]
async fn slow_tasks_empty() {
    let store = TaskStore::open_memory().await.unwrap();
    let slow = store.get_slow_tasks_7d().await.unwrap();
    assert!(slow.is_empty());
}

#[tokio::test]
async fn error_distribution_empty() {
    let store = TaskStore::open_memory().await.unwrap();
    let errors = store.get_error_distribution(24 * 7).await.unwrap();
    assert!(errors.is_empty());
}

#[tokio::test]
async fn high_review_cycle_tasks_empty() {
    let store = TaskStore::open_memory().await.unwrap();
    let tasks = store.get_high_review_cycle_tasks_7d().await.unwrap();
    assert!(tasks.is_empty());
}

#[tokio::test]
async fn high_review_cycle_tasks_filters_by_threshold() {
    let store = TaskStore::open_memory().await.unwrap();

    // Create tasks with different review_cycles values
    let id_low = store
        .create_internal("owner/repo", "Low cycles", "", "manual", "", None)
        .await
        .unwrap();
    let id_high = store
        .create_internal("owner/repo", "High cycles", "", "manual", "", None)
        .await
        .unwrap();
    let id_boundary = store
        .create_internal("owner/repo", "At boundary", "", "manual", "", None)
        .await
        .unwrap();

    // Set review_cycles: 0, 2, 3
    store.increment(id_low, "review_cycles").await.unwrap();
    // id_low has review_cycles=1, should be excluded

    store.increment(id_high, "review_cycles").await.unwrap();
    store.increment(id_high, "review_cycles").await.unwrap();
    store.increment(id_high, "review_cycles").await.unwrap();
    // id_high has review_cycles=3, should be included

    store.increment(id_boundary, "review_cycles").await.unwrap();
    store.increment(id_boundary, "review_cycles").await.unwrap();
    // id_boundary has review_cycles=2, should be included (>=2)

    let tasks = store.get_high_review_cycle_tasks_7d().await.unwrap();
    assert_eq!(tasks.len(), 2);
    // Should be sorted by review_cycles DESC
    assert_eq!(tasks[0].title, "High cycles");
    assert_eq!(tasks[0].review_cycles, 3);
    assert_eq!(tasks[1].title, "At boundary");
    assert_eq!(tasks[1].review_cycles, 2);
}

#[tokio::test]
async fn high_review_cycle_tasks_excludes_old_tasks() {
    let store = TaskStore::open_memory().await.unwrap();

    let id_old = store
        .create_internal("owner/repo", "Old task", "", "manual", "", None)
        .await
        .unwrap();
    store.increment(id_old, "review_cycles").await.unwrap();
    store.increment(id_old, "review_cycles").await.unwrap();

    // Backdate to 10 days ago (outside 7d window)
    sqlx::query("UPDATE tasks SET updated_at = datetime('now', '-10 days') WHERE id = ?")
        .bind(id_old)
        .execute(&store.pool)
        .await
        .unwrap();

    let id_recent = store
        .create_internal("owner/repo", "Recent task", "", "manual", "", None)
        .await
        .unwrap();
    store.increment(id_recent, "review_cycles").await.unwrap();
    store.increment(id_recent, "review_cycles").await.unwrap();

    let tasks = store.get_high_review_cycle_tasks_7d().await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title, "Recent task");
}

#[tokio::test]
async fn error_distribution_groups_by_type() {
    use chrono::Utc;
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();

    for error_type in &["timeout", "timeout", "rate_limit"] {
        store
            .insert_task_metric(&InsertTaskMetric {
                repo: "",
                task_id: "1",
                agent: "claude",
                model: None,
                complexity: None,
                outcome: "failed",
                duration_seconds: 10.0,
                started_at: &now,
                completed_at: &now,
                attempts: 1,
                files_changed: 0,
                error_type: Some(error_type),
                input_tokens: None,
                output_tokens: None,
                input_cost_usd: None,
                output_cost_usd: None,
                total_cost_usd: None,
            })
            .await
            .unwrap();
    }

    let errors = store.get_error_distribution(24 * 7).await.unwrap();
    assert_eq!(errors.len(), 2);
    // timeout should be first (count=2)
    assert_eq!(errors[0].error_type.as_deref(), Some("timeout"));
    assert_eq!(errors[0].count, 2);
}

#[tokio::test]
async fn slow_tasks_returns_sorted_by_duration() {
    use chrono::Utc;
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();

    // Insert three metrics with different durations
    for (task_id, duration) in &[("t1", 120.0), ("t2", 300.0), ("t3", 60.0)] {
        store
            .insert_task_metric(&InsertTaskMetric {
                repo: "",
                task_id,
                agent: "claude",
                model: Some("sonnet"),
                complexity: Some("medium"),
                outcome: "success",
                duration_seconds: *duration,
                started_at: &now,
                completed_at: &now,
                attempts: 1,
                files_changed: 1,
                error_type: None,
                input_tokens: None,
                output_tokens: None,
                input_cost_usd: None,
                output_cost_usd: None,
                total_cost_usd: None,
            })
            .await
            .unwrap();
    }

    let slow = store.get_slow_tasks(24 * 7).await.unwrap();
    assert_eq!(slow.len(), 3);
    // Should be sorted descending by duration
    assert_eq!(slow[0].task_id, "t2");
    assert!((slow[0].duration_seconds - 300.0).abs() < 0.01);
    assert_eq!(slow[1].task_id, "t1");
    assert_eq!(slow[2].task_id, "t3");
}

// ── pricing_for_model ──────────────────────────────────────────────

#[test]
fn pricing_known_models() {
    use super::pricing_for_model;

    let opus = pricing_for_model("claude-opus-4-6");
    assert!((opus.input_per_million_usd - 15.0).abs() < 0.01);
    assert!((opus.output_per_million_usd - 75.0).abs() < 0.01);

    let sonnet = pricing_for_model("claude-sonnet-4-6");
    assert!((sonnet.input_per_million_usd - 3.0).abs() < 0.01);

    let haiku = pricing_for_model("Haiku");
    assert!((haiku.input_per_million_usd - 0.8).abs() < 0.01);

    let o3 = pricing_for_model("o3");
    assert!((o3.input_per_million_usd - 2.0).abs() < 0.01);

    let gpt_mini = pricing_for_model("o4-mini");
    assert!((gpt_mini.input_per_million_usd - 0.15).abs() < 0.01);

    let gpt41 = pricing_for_model("gpt-4.1");
    assert!((gpt41.input_per_million_usd - 2.0).abs() < 0.01);
}

#[test]
fn pricing_unknown_model_returns_fallback() {
    use super::pricing_for_model;
    let unknown = pricing_for_model("totally-unknown-model");
    assert!((unknown.input_per_million_usd - 1.0).abs() < 0.01);
    assert!((unknown.output_per_million_usd - 4.0).abs() < 0.01);
}

#[test]
fn pricing_free_and_subscription_models() {
    use super::pricing_for_model;

    // GitHub Copilot subscription — $0
    let copilot_sonnet = pricing_for_model("github-copilot/claude-sonnet-4-6");
    assert_eq!(copilot_sonnet.input_per_million_usd, 0.0);
    assert_eq!(copilot_sonnet.output_per_million_usd, 0.0);

    let copilot_gpt = pricing_for_model("github-copilot/gpt-5.4-mini");
    assert_eq!(copilot_gpt.input_per_million_usd, 0.0);

    // Free-tier suffix — $0
    let minimax_free = pricing_for_model("minimax-m2.5-free");
    assert_eq!(minimax_free.input_per_million_usd, 0.0);

    let nemotron_free = pricing_for_model("nemotron-3-super-free");
    assert_eq!(nemotron_free.input_per_million_usd, 0.0);

    // DeepSeek
    let deepseek = pricing_for_model("deepseek-r1");
    assert!((deepseek.input_per_million_usd - 0.55).abs() < 0.01);
    assert!((deepseek.output_per_million_usd - 2.19).abs() < 0.01);

    // Codex v5 mini
    let codex_mini = pricing_for_model("gpt-5.1-codex-mini");
    assert!((codex_mini.input_per_million_usd - 0.15).abs() < 0.01);
    assert!((codex_mini.output_per_million_usd - 0.6).abs() < 0.01);

    // Codex v5 full
    let codex_full = pricing_for_model("gpt-5.2-codex");
    assert!((codex_full.input_per_million_usd - 2.0).abs() < 0.01);
    assert!((codex_full.output_per_million_usd - 8.0).abs() < 0.01);
}

// ── cost_summary static queries ──────────────────────────────────────

#[tokio::test]
async fn cost_summary_returns_three_periods() {
    use chrono::Utc;
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();

    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "",
            task_id: "cost1",
            agent: "claude",
            model: Some("sonnet"),
            complexity: None,
            outcome: "success",
            duration_seconds: 10.0,
            started_at: &now,
            completed_at: &now,
            attempts: 1,
            files_changed: 1,
            error_type: None,
            input_tokens: Some(1000),
            output_tokens: Some(500),
            input_cost_usd: Some(0.003),
            output_cost_usd: Some(0.0075),
            total_cost_usd: Some(0.0105),
        })
        .await
        .unwrap();

    let summary = store.get_cost_summary().await.unwrap();
    assert_eq!(summary.periods.len(), 3);
    assert_eq!(summary.periods[0].label, "24h");
    assert_eq!(summary.periods[1].label, "7d");
    assert_eq!(summary.periods[2].label, "30d");
    // All three windows should include our recent metric
    for period in &summary.periods {
        assert_eq!(period.task_count, 1);
        assert!(period.total_cost_usd > 0.0);
    }
}

// ── has_external_tasks ───────────────────────────────────────────

#[tokio::test]
async fn has_external_tasks_returns_false_for_empty_repo() {
    let store = TaskStore::open_memory().await.unwrap();
    assert!(!store.has_external_tasks("owner/repo").await);
}

#[tokio::test]
async fn has_external_tasks_returns_true_after_external_insert() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .upsert_external(&UpsertExternal {
            repo: "owner/repo",
            ext_id: "1",
            title: "task",
            body: "body",
            author: "user",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    assert!(store.has_external_tasks("owner/repo").await);
    // Different repo should still be false
    assert!(!store.has_external_tasks("other/repo").await);
}

#[tokio::test]
async fn has_external_tasks_ignores_internal_rows() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .create_internal("owner/repo", "task", "body", "manual", "", None)
        .await
        .unwrap();

    assert!(!store.has_external_tasks("owner/repo").await);

    store
        .upsert_external(&UpsertExternal {
            repo: "owner/repo",
            ext_id: "42",
            title: "External",
            body: "",
            author: "user",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();

    assert!(store.has_external_tasks("owner/repo").await);
}

#[tokio::test]
async fn list_external_scopes_ignore_internal_rows() {
    let store = TaskStore::open_memory().await.unwrap();

    let internal_id = store
        .create_internal("owner/repo", "internal", "", "manual", "", None)
        .await
        .unwrap();
    store
        .update_status(internal_id, TaskStatus::Routed)
        .await
        .unwrap();

    let external_id = store
        .upsert_external(&UpsertExternal {
            repo: "owner/repo",
            ext_id: "99",
            title: "external",
            body: "",
            author: "user",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .update_status(external_id, TaskStatus::Routed)
        .await
        .unwrap();

    let routed = store
        .list_external_by_status("owner/repo", TaskStatus::Routed)
        .await
        .unwrap();
    let all = store.list_all_external("owner/repo").await.unwrap();

    assert_eq!(routed.len(), 1);
    assert_eq!(routed[0].external_id.as_deref(), Some("99"));
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].external_id.as_deref(), Some("99"));
}

// ── list_cleanable ──────────────────────────────────────────────

#[tokio::test]
async fn list_cleanable_returns_done_with_worktree() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create_internal("owner/repo", "task", "body", "manual", "", None)
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Done).await.unwrap();
    store
        .set_fields(id, &[("worktree", serde_json::json!("/tmp/wt"))])
        .await
        .unwrap();

    let cleanable = store.list_cleanable("owner/repo").await.unwrap();
    assert_eq!(cleanable.len(), 1);
    assert_eq!(cleanable[0].id, id);
}

#[tokio::test]
async fn list_cleanable_excludes_already_cleaned() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create_internal("owner/repo", "task", "body", "manual", "", None)
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Done).await.unwrap();
    store
        .set_fields(id, &[("worktree", serde_json::json!("/tmp/wt"))])
        .await
        .unwrap();
    store.mark_cleaned(id).await.unwrap();

    let cleanable = store.list_cleanable("owner/repo").await.unwrap();
    assert!(cleanable.is_empty());
}

// ── get_last_run ────────────────────────────────────────────────

#[tokio::test]
async fn get_last_run_returns_most_recent() {
    let store = TaskStore::open_memory().await.unwrap();
    let task_id = store
        .create_internal("owner/repo", "task", "body", "manual", "", None)
        .await
        .unwrap();

    // Create two runs
    let run1 = StartRun {
        task_id,
        attempt: 1,
        run_type: "agent",
        agent: "claude",
        model: "sonnet",
        command: "cmd1",
        prompt: "p1",
    };
    store.start_run(&run1).await.unwrap();

    let run2 = StartRun {
        task_id,
        attempt: 2,
        run_type: "agent",
        agent: "claude",
        model: "opus",
        command: "cmd2",
        prompt: "p2",
    };
    store.start_run(&run2).await.unwrap();

    let last = store.get_last_run(task_id, "agent").await.unwrap().unwrap();
    assert_eq!(last.attempt, 2);
    assert_eq!(last.model, "opus");
}

#[tokio::test]
async fn get_last_run_filters_by_type() {
    let store = TaskStore::open_memory().await.unwrap();
    let task_id = store
        .create_internal("owner/repo", "task", "body", "manual", "", None)
        .await
        .unwrap();

    let agent_run = StartRun {
        task_id,
        attempt: 1,
        run_type: "agent",
        agent: "claude",
        model: "sonnet",
        command: "",
        prompt: "",
    };
    store.start_run(&agent_run).await.unwrap();

    let review_run = StartRun {
        task_id,
        attempt: 1,
        run_type: "review",
        agent: "codex",
        model: "gpt-5",
        command: "",
        prompt: "",
    };
    store.start_run(&review_run).await.unwrap();

    let last_review = store
        .get_last_run(task_id, "review")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last_review.agent, "codex");
    assert_eq!(last_review.run_type, "review");

    // No route run exists
    let no_route = store.get_last_run(task_id, "route").await.unwrap();
    assert!(no_route.is_none());
}

// ── prune_old_runs ──────────────────────────────────────────────

#[tokio::test]
async fn prune_old_runs_removes_completed_task_runs() {
    let store = TaskStore::open_memory().await.unwrap();
    let task_id = store
        .create_internal("owner/repo", "task", "body", "manual", "", None)
        .await
        .unwrap();
    store
        .update_status(task_id, TaskStatus::Done)
        .await
        .unwrap();
    // Backdate the task so it appears old
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(task_id)
        .execute(&store.pool)
        .await
        .unwrap();

    let run = StartRun {
        task_id,
        attempt: 1,
        run_type: "agent",
        agent: "claude",
        model: "sonnet",
        command: "",
        prompt: "",
    };
    store.start_run(&run).await.unwrap();

    let pruned = store.prune_old_runs(30).await.unwrap();
    assert_eq!(pruned, 1);
    assert!(store.get_runs(task_id).await.unwrap().is_empty());
}

#[tokio::test]
async fn prune_old_runs_keeps_recent_task_runs() {
    let store = TaskStore::open_memory().await.unwrap();
    let task_id = store
        .create_internal("owner/repo", "task", "body", "manual", "", None)
        .await
        .unwrap();
    store
        .update_status(task_id, TaskStatus::Done)
        .await
        .unwrap();
    // Task stays with recent updated_at (default = now)

    let run = StartRun {
        task_id,
        attempt: 1,
        run_type: "agent",
        agent: "claude",
        model: "sonnet",
        command: "",
        prompt: "",
    };
    store.start_run(&run).await.unwrap();

    let pruned = store.prune_old_runs(30).await.unwrap();
    assert_eq!(pruned, 0);
    assert_eq!(store.get_runs(task_id).await.unwrap().len(), 1);
}

// ── increment named column alias ────────────────────────────────

#[tokio::test]
async fn increment_returns_new_value_via_named_column() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create_internal("owner/repo", "task", "body", "manual", "", None)
        .await
        .unwrap();

    let v1 = store.increment(id, "attempts").await.unwrap();
    assert_eq!(v1, 1);
    let v2 = store.increment(id, "attempts").await.unwrap();
    assert_eq!(v2, 2);
}

#[tokio::test]
async fn subscribe_and_list_channel_subscriptions() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .subscribe_channel("telegram", "42", "owner/orch", None)
        .await
        .unwrap();
    store
        .subscribe_channel("telegram", "42", "owner/bean", None)
        .await
        .unwrap();
    let subs = store
        .list_channel_subscriptions("telegram", "42")
        .await
        .unwrap();
    assert_eq!(subs.len(), 2);
    assert!(subs.contains(&"owner/orch".to_string()));
    assert!(subs.contains(&"owner/bean".to_string()));
}

#[tokio::test]
async fn unsubscribe_channel() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .subscribe_channel("telegram", "42", "owner/orch", None)
        .await
        .unwrap();
    store
        .unsubscribe_channel("telegram", "42", "owner/orch")
        .await
        .unwrap();
    let subs = store
        .list_channel_subscriptions("telegram", "42")
        .await
        .unwrap();
    assert_eq!(subs.len(), 0);
}

#[tokio::test]
async fn subscribe_idempotent() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .subscribe_channel("telegram", "42", "owner/orch", None)
        .await
        .unwrap();
    store
        .subscribe_channel("telegram", "42", "owner/orch", None)
        .await
        .unwrap(); // duplicate
    let subs = store
        .list_channel_subscriptions("telegram", "42")
        .await
        .unwrap();
    assert_eq!(subs.len(), 1);
}

#[tokio::test]
async fn list_subscribers_for_repo() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .subscribe_channel("telegram", "42", "owner/orch", None)
        .await
        .unwrap();
    store
        .subscribe_channel("discord", "99", "owner/orch", None)
        .await
        .unwrap();
    store
        .subscribe_channel("telegram", "42", "owner/bean", None)
        .await
        .unwrap();
    let subs = store.list_subscribers_for_repo("owner/orch").await.unwrap();
    assert_eq!(subs.len(), 2);
}

#[tokio::test]
async fn metrics_summary_by_repo_filters_correctly() {
    let store = TaskStore::open_memory().await.unwrap();

    // Create tasks in different repos
    let id1 = store
        .create_internal("owner/orch", "Task A", "", "test", "", None)
        .await
        .unwrap();
    let id2 = store
        .create_internal("owner/bean", "Task B", "", "test", "", None)
        .await
        .unwrap();

    let now = chrono::Utc::now();
    let ago = now - chrono::Duration::minutes(30);

    // Insert metric for orch
    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "owner/orch",
            task_id: &id1.to_string(),
            agent: "claude",
            model: Some("sonnet"),
            complexity: Some("simple"),
            outcome: "success",
            duration_seconds: 60.0,
            started_at: &ago,
            completed_at: &now,
            attempts: 1,
            files_changed: 2,
            error_type: None,
            input_tokens: None,
            output_tokens: None,
            input_cost_usd: None,
            output_cost_usd: None,
            total_cost_usd: None,
        })
        .await
        .unwrap();

    // Insert metric for bean
    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "owner/bean",
            task_id: &id2.to_string(),
            agent: "codex",
            model: Some("gpt-5"),
            complexity: Some("medium"),
            outcome: "success",
            duration_seconds: 120.0,
            started_at: &ago,
            completed_at: &now,
            attempts: 1,
            files_changed: 1,
            error_type: None,
            input_tokens: None,
            output_tokens: None,
            input_cost_usd: None,
            output_cost_usd: None,
            total_cost_usd: None,
        })
        .await
        .unwrap();

    // Query per-repo: orch
    let orch_stats = store
        .get_metrics_summary_by_repo("owner/orch", 24)
        .await
        .unwrap();
    assert_eq!(orch_stats.tasks_completed_24h, 1);
    assert_eq!(orch_stats.agent_stats.len(), 1);
    assert_eq!(orch_stats.agent_stats[0].agent, "claude");

    // Query per-repo: bean
    let bean_stats = store
        .get_metrics_summary_by_repo("owner/bean", 24)
        .await
        .unwrap();
    assert_eq!(bean_stats.tasks_completed_24h, 1);
    assert_eq!(bean_stats.agent_stats.len(), 1);
    assert_eq!(bean_stats.agent_stats[0].agent, "codex");

    // Global should show both
    let all_stats = store.get_metrics_summary_24h().await.unwrap();
    assert_eq!(all_stats.tasks_completed_24h, 2);
}

#[tokio::test]
async fn subscription_round_trip_with_multiple_channels() {
    let store = TaskStore::open_memory().await.unwrap();

    // Subscribe telegram and discord to orch
    store
        .subscribe_channel("telegram", "42", "owner/orch", None)
        .await
        .unwrap();
    store
        .subscribe_channel("discord", "1111", "owner/orch", None)
        .await
        .unwrap();

    // Subscribe telegram to bean too
    store
        .subscribe_channel("telegram", "42", "owner/bean", None)
        .await
        .unwrap();

    // Check subscribers for orch
    let orch_subs = store.list_subscribers_for_repo("owner/orch").await.unwrap();
    assert_eq!(orch_subs.len(), 2);

    // Check what telegram:42 is subscribed to
    let tg_subs = store
        .list_channel_subscriptions("telegram", "42")
        .await
        .unwrap();
    assert_eq!(tg_subs.len(), 2);
    assert!(tg_subs.contains(&"owner/orch".to_string()));
    assert!(tg_subs.contains(&"owner/bean".to_string()));

    // Unsubscribe orch from telegram
    store
        .unsubscribe_channel("telegram", "42", "owner/orch")
        .await
        .unwrap();

    let tg_subs = store
        .list_channel_subscriptions("telegram", "42")
        .await
        .unwrap();
    assert_eq!(tg_subs.len(), 1);
    assert_eq!(tg_subs[0], "owner/bean");

    // orch should now only have discord subscriber
    let orch_subs = store.list_subscribers_for_repo("owner/orch").await.unwrap();
    assert_eq!(orch_subs.len(), 1);
    assert_eq!(
        orch_subs[0],
        ("discord".to_string(), "1111".to_string(), "".to_string())
    );
}

#[tokio::test]
async fn subscribe_with_topic_id_preserved() {
    let store = TaskStore::open_memory().await.unwrap();

    // Subscribe to orch with a topic_id (e.g., Telegram forum topic)
    store
        .subscribe_channel("telegram", "42", "owner/orch", Some("topic-123"))
        .await
        .unwrap();

    // Subscribe to bean without a topic (root thread)
    store
        .subscribe_channel("telegram", "42", "owner/bean", None)
        .await
        .unwrap();

    // List subscribers for orch - should include topic_id
    let orch_subs = store.list_subscribers_for_repo("owner/orch").await.unwrap();
    assert_eq!(orch_subs.len(), 1);
    assert_eq!(
        orch_subs[0],
        (
            "telegram".to_string(),
            "42".to_string(),
            "topic-123".to_string()
        )
    );

    // List subscribers for bean - should have empty topic_id
    let bean_subs = store.list_subscribers_for_repo("owner/bean").await.unwrap();
    assert_eq!(bean_subs.len(), 1);
    assert_eq!(
        bean_subs[0],
        ("telegram".to_string(), "42".to_string(), "".to_string())
    );
}

// ── get_cost_summary_by_repo ───────────────────────────────────

#[tokio::test]
async fn cost_summary_by_repo_isolates_per_repo() {
    use chrono::Utc;
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();

    // Insert a metric for owner/orch
    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "owner/orch",
            task_id: "orch-1",
            agent: "claude",
            model: Some("sonnet"),
            complexity: None,
            outcome: "success",
            duration_seconds: 10.0,
            started_at: &now,
            completed_at: &now,
            attempts: 1,
            files_changed: 1,
            error_type: None,
            input_tokens: Some(1000),
            output_tokens: Some(500),
            input_cost_usd: Some(0.003),
            output_cost_usd: Some(0.0075),
            total_cost_usd: Some(0.0105),
        })
        .await
        .unwrap();

    // Insert a metric for owner/bean
    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "owner/bean",
            task_id: "bean-1",
            agent: "codex",
            model: Some("gpt-4"),
            complexity: None,
            outcome: "success",
            duration_seconds: 20.0,
            started_at: &now,
            completed_at: &now,
            attempts: 1,
            files_changed: 2,
            error_type: None,
            input_tokens: Some(2000),
            output_tokens: Some(1000),
            input_cost_usd: Some(0.006),
            output_cost_usd: Some(0.015),
            total_cost_usd: Some(0.021),
        })
        .await
        .unwrap();

    // owner/orch should only see its own cost
    let orch_cost = store
        .get_cost_summary_by_repo("owner/orch", 24)
        .await
        .unwrap();
    assert_eq!(orch_cost.periods.len(), 1);
    assert_eq!(orch_cost.periods[0].label, "24h");
    assert_eq!(orch_cost.periods[0].task_count, 1);
    assert!((orch_cost.periods[0].total_cost_usd - 0.0105).abs() < 1e-6);

    // owner/bean should only see its own cost
    let bean_cost = store
        .get_cost_summary_by_repo("owner/bean", 24)
        .await
        .unwrap();
    assert_eq!(bean_cost.periods[0].task_count, 1);
    assert!((bean_cost.periods[0].total_cost_usd - 0.021).abs() < 1e-6);

    // Unknown repo returns zeros
    let unknown = store
        .get_cost_summary_by_repo("unknown/repo", 24)
        .await
        .unwrap();
    assert_eq!(unknown.periods[0].task_count, 0);
    assert_eq!(unknown.periods[0].total_cost_usd, 0.0);
}

#[tokio::test]
async fn cost_summary_by_repo_falls_back_to_tasks_join() {
    use chrono::Utc;
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();

    // Create a task with a known repo so the join can resolve it
    let task_id = store
        .create_internal("owner/orch", "task", "body", "manual", "", None)
        .await
        .unwrap();

    // Insert metric with empty repo — repo will be resolved via tasks join
    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "",
            task_id: &task_id.to_string(),
            agent: "claude",
            model: None,
            complexity: None,
            outcome: "success",
            duration_seconds: 5.0,
            started_at: &now,
            completed_at: &now,
            attempts: 1,
            files_changed: 0,
            error_type: None,
            input_tokens: Some(500),
            output_tokens: Some(200),
            input_cost_usd: Some(0.0015),
            output_cost_usd: Some(0.003),
            total_cost_usd: Some(0.0045),
        })
        .await
        .unwrap();

    let cost = store
        .get_cost_summary_by_repo("owner/orch", 24)
        .await
        .unwrap();
    assert_eq!(cost.periods[0].task_count, 1);
    assert!((cost.periods[0].total_cost_usd - 0.0045).abs() < 1e-6);
}

#[tokio::test]
async fn cost_summary_by_repo_uses_requested_window_label() {
    use chrono::{Duration, Utc};
    let store = TaskStore::open_memory().await.unwrap();
    let now = Utc::now();
    let eight_days_ago = now - Duration::days(8);

    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "owner/orch",
            task_id: "recent",
            agent: "claude",
            model: Some("sonnet"),
            complexity: None,
            outcome: "success",
            duration_seconds: 10.0,
            started_at: &now,
            completed_at: &now,
            attempts: 1,
            files_changed: 1,
            error_type: None,
            input_tokens: Some(100),
            output_tokens: Some(50),
            input_cost_usd: Some(0.001),
            output_cost_usd: Some(0.002),
            total_cost_usd: Some(0.003),
        })
        .await
        .unwrap();

    store
        .insert_task_metric(&InsertTaskMetric {
            repo: "owner/orch",
            task_id: "older",
            agent: "claude",
            model: Some("sonnet"),
            complexity: None,
            outcome: "success",
            duration_seconds: 12.0,
            started_at: &eight_days_ago,
            completed_at: &eight_days_ago,
            attempts: 1,
            files_changed: 1,
            error_type: None,
            input_tokens: Some(200),
            output_tokens: Some(100),
            input_cost_usd: Some(0.002),
            output_cost_usd: Some(0.004),
            total_cost_usd: Some(0.006),
        })
        .await
        .unwrap();

    let summary_7d = store
        .get_cost_summary_by_repo("owner/orch", 24 * 7)
        .await
        .unwrap();
    assert_eq!(summary_7d.periods[0].label, "7d");
    assert_eq!(summary_7d.periods[0].task_count, 1);
    assert!((summary_7d.periods[0].total_cost_usd - 0.003).abs() < 1e-6);

    let summary_30d = store
        .get_cost_summary_by_repo("owner/orch", 24 * 30)
        .await
        .unwrap();
    assert_eq!(summary_30d.periods[0].label, "30d");
    assert_eq!(summary_30d.periods[0].task_count, 2);
    assert!((summary_30d.periods[0].total_cost_usd - 0.009).abs() < 1e-6);
}

#[tokio::test]
async fn control_insert_and_list() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .insert_control_message(
            "default",
            "user",
            "cli",
            None,
            "what's running?",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    store
        .insert_control_message(
            "default",
            "assistant",
            "cli",
            None,
            "3 tasks active",
            Some("listed tasks"),
            Some("sonnet"),
            Some("claude"),
            Some(300),
            Some(200),
            Some(500),
            Some(0.01),
        )
        .await
        .unwrap();

    let messages = store
        .list_control_messages("default", None, 10)
        .await
        .unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(messages[1].summary.as_deref(), Some("listed tasks"));
}

#[tokio::test]
async fn control_search_messages() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .insert_control_message(
            "default",
            "user",
            "cli",
            None,
            "check bean auth issue",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    store
        .insert_control_message(
            "default",
            "user",
            "cli",
            None,
            "unblock trading tasks",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    let results = store
        .search_control_messages("default", "bean", None, 10)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].content.contains("bean"));
}

#[tokio::test]
async fn control_recent_summaries() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .insert_control_message(
            "default",
            "assistant",
            "cli",
            None,
            "long response",
            Some("did X"),
            Some("sonnet"),
            Some("claude"),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    store
        .insert_control_message(
            "default",
            "assistant",
            "cli",
            None,
            "another response",
            Some("did Y"),
            Some("sonnet"),
            Some("claude"),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    let summaries = store.control_recent_summaries("default", 5).await.unwrap();
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0], "did X");
    assert_eq!(summaries[1], "did Y");
}

#[tokio::test]
async fn control_sessions_are_isolated() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .insert_control_message(
            "session-a",
            "user",
            "cli",
            None,
            "message in A",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    store
        .insert_control_message(
            "session-b",
            "user",
            "cli",
            None,
            "message in B",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    let a_msgs = store
        .list_control_messages("session-a", None, 10)
        .await
        .unwrap();
    let b_msgs = store
        .list_control_messages("session-b", None, 10)
        .await
        .unwrap();
    assert_eq!(a_msgs.len(), 1);
    assert_eq!(b_msgs.len(), 1);
    assert!(a_msgs[0].content.contains("in A"));
    assert!(b_msgs[0].content.contains("in B"));
}

#[tokio::test]
async fn control_session_cost_summary_aggregates_tokens_cost_and_model_breakdown() {
    let store = TaskStore::open_memory().await.unwrap();

    store
        .insert_control_message(
            "default", "user", "cli", None, "hello", None, None, None, None, None, None, None,
        )
        .await
        .unwrap();

    store
        .insert_control_message(
            "default",
            "assistant",
            "cli",
            None,
            "hi",
            Some("responded"),
            Some("sonnet"),
            Some("claude"),
            Some(100),
            Some(40),
            Some(140),
            Some(0.010),
        )
        .await
        .unwrap();

    store
        .insert_control_message(
            "default",
            "assistant",
            "cli",
            None,
            "done",
            Some("completed"),
            Some("sonnet"),
            Some("claude"),
            Some(50),
            Some(25),
            Some(75),
            Some(0.005),
        )
        .await
        .unwrap();

    let summary = store.get_session_cost_summary("default").await.unwrap();
    assert_eq!(summary.total_messages, 3);
    assert_eq!(summary.assistant_messages, 2);
    assert_eq!(summary.total_input_tokens, 150);
    assert_eq!(summary.total_output_tokens, 65);
    assert_eq!(summary.total_tokens, 215);
    assert!((summary.total_cost_usd - 0.015).abs() < 1e-9);
    assert_eq!(summary.primary_model.as_deref(), Some("sonnet"));
    assert_eq!(summary.primary_agent.as_deref(), Some("claude"));
    assert_eq!(summary.by_model.len(), 1);
    assert_eq!(summary.by_model[0].0, "sonnet");
    assert_eq!(summary.by_model[0].1, 2);
    assert_eq!(summary.by_model[0].2, 215);
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
        "fallback must not resolve a task from a different repo (got {:?})",
        resolved
    );
}

// ── parse_since_duration ──────────────────────────────────────────────────────

#[test]
fn parse_since_duration_days() {
    use crate::store::control::parse_since_duration;
    let ts = parse_since_duration("7d").unwrap();
    // The timestamp must be a well-formed RFC3339 string (e.g. 2026-03-18T09:56:02Z).
    assert_eq!(ts.len(), 20, "unexpected length: {ts:?}");
    // Must be in RFC3339 format: starts with year, contains 'T', ends with 'Z'.
    assert!(
        ts.starts_with("20") && ts.contains('T') && ts.ends_with('Z'),
        "expected RFC3339 format: {ts:?}"
    );
    // Must be in the past (less than now).
    let now_ts = parse_since_duration("0d");
    assert!(now_ts.is_err(), "0d should be rejected");
}

#[test]
fn parse_since_duration_hours() {
    use crate::store::control::parse_since_duration;
    let ts = parse_since_duration("24h").unwrap();
    assert_eq!(ts.len(), 20);
}

#[test]
fn parse_since_duration_minutes() {
    use crate::store::control::parse_since_duration;
    let ts = parse_since_duration("30m").unwrap();
    assert_eq!(ts.len(), 20);
}

#[test]
fn parse_since_duration_invalid() {
    use crate::store::control::parse_since_duration;
    assert!(parse_since_duration("abc").is_err());
    assert!(parse_since_duration("7").is_err());
    assert!(parse_since_duration("").is_err());
    assert!(parse_since_duration("0h").is_err());
}

// ── list_control_messages with since ─────────────────────────────────────────

#[tokio::test]
async fn control_list_messages_since_filters_old() {
    use crate::store::control::parse_since_duration;

    let store = TaskStore::open_memory().await.unwrap();

    // Insert a message with a timestamp far in the past.
    sqlx::query(
        "INSERT INTO control_messages
         (session_id, role, channel, content, created_at)
         VALUES ('default', 'user', 'cli', 'old message', '2020-01-01 00:00:00')",
    )
    .execute(store.pool())
    .await
    .unwrap();

    // Insert a recent message using the normal API (created_at defaults to NOW).
    store
        .insert_control_message(
            "default",
            "user",
            "cli",
            None,
            "recent message",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    // Without a since filter: both messages returned.
    let all = store
        .list_control_messages("default", None, 10)
        .await
        .unwrap();
    assert_eq!(all.len(), 2, "expected 2 messages without filter");

    // With a since filter (7 days ago): only the recent one should appear.
    let since_ts = parse_since_duration("7d").unwrap();
    let filtered = store
        .list_control_messages("default", Some(&since_ts), 10)
        .await
        .unwrap();
    assert_eq!(
        filtered.len(),
        1,
        "expected 1 message after filtering, got: {filtered:?}"
    );
    assert!(filtered[0].content.contains("recent"));
}

// ── search_control_messages with since ───────────────────────────────────────

#[tokio::test]
async fn control_search_messages_since_filters_old() {
    use crate::store::control::parse_since_duration;

    let store = TaskStore::open_memory().await.unwrap();

    // Insert an old matching message.
    sqlx::query(
        "INSERT INTO control_messages
         (session_id, role, channel, content, created_at)
         VALUES ('default', 'user', 'cli', 'bean auth old', '2020-01-01 00:00:00')",
    )
    .execute(store.pool())
    .await
    .unwrap();

    // Insert a recent matching message.
    store
        .insert_control_message(
            "default",
            "user",
            "cli",
            None,
            "bean auth recent",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    // Insert a recent non-matching message.
    store
        .insert_control_message(
            "default",
            "user",
            "cli",
            None,
            "unrelated recent",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    // Search without since: both bean messages returned.
    let all_bean = store
        .search_control_messages("default", "bean", None, 10)
        .await
        .unwrap();
    assert_eq!(all_bean.len(), 2, "expected 2 bean matches without filter");

    // Search with since: only the recent bean message returned.
    let since_ts = parse_since_duration("7d").unwrap();
    let recent_bean = store
        .search_control_messages("default", "bean", Some(&since_ts), 10)
        .await
        .unwrap();
    assert_eq!(
        recent_bean.len(),
        1,
        "expected 1 bean match after since filter, got: {recent_bean:?}"
    );
    assert!(recent_bean[0].content.contains("recent"));
}

/// Verify that migrations run cleanly on a fresh database.
///
/// This catches the most common agent mistake: modifying an existing migration
/// file instead of creating a new one. SQLx checksums are immutable — if a
/// migration file changes after it was applied, this test fails.
#[tokio::test]
async fn migrations_run_on_fresh_db() {
    let result = TaskStore::open_memory().await;
    if let Err(e) = &result {
        panic!("migrations failed on fresh DB: {e}");
    }
}

/// Verify that migrations are idempotent — running them twice on the same
/// database must succeed (no checksum mismatches).
#[tokio::test]
async fn migrations_are_idempotent() {
    // Use a file-based temp DB so we can close and reopen it
    let tmp = std::env::temp_dir().join(format!("orch-migration-test-{}.db", std::process::id()));

    // First open: runs all migrations
    {
        let result = TaskStore::open(&tmp).await;
        if let Err(e) = &result {
            panic!("first migration run failed: {e}");
        }
    }

    // Second open: re-validates checksums against already-applied migrations
    {
        let result = TaskStore::open(&tmp).await;
        if let Err(e) = &result {
            panic!("second migration run failed (checksum mismatch?): {e}");
        }
    }

    // Cleanup
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(tmp.with_extension("db-shm"));
    let _ = std::fs::remove_file(tmp.with_extension("db-wal"));
}

// ── Recent rate limit counts (health check query) ───────────────

#[tokio::test]
async fn recent_rate_limit_counts_empty() {
    let store = TaskStore::open_memory().await.unwrap();
    let counts = store.recent_rate_limit_counts(6).await.unwrap();
    assert!(counts.is_empty());
}

#[tokio::test]
async fn recent_rate_limit_counts_groups_by_agent() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .record_rate_limit("claude", "rate_limit", None)
        .await
        .unwrap();
    store
        .record_rate_limit("claude", "out_of_credits", None)
        .await
        .unwrap();
    store
        .record_rate_limit("codex", "rate_limit", None)
        .await
        .unwrap();

    let counts = store.recent_rate_limit_counts(6).await.unwrap();
    assert_eq!(counts.get("claude"), Some(&2));
    assert_eq!(counts.get("codex"), Some(&1));
    assert_eq!(counts.get("opencode"), None);
}

// ---------------------------------------------------------------------------
// Batch methods
// ---------------------------------------------------------------------------

async fn make_task(store: &TaskStore) -> i64 {
    store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Batch test".to_string(),
            body: "".to_string(),
            source: "manual".to_string(),
            source_id: "".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn batch_set_fields_updates_multiple_tasks() {
    let store = TaskStore::open_memory().await.unwrap();
    let id1 = make_task(&store).await;
    let id2 = make_task(&store).await;

    let updates1: &[(&str, serde_json::Value)] = &[("summary", serde_json::json!("first"))];
    let updates2: &[(&str, serde_json::Value)] = &[("summary", serde_json::json!("second"))];
    store
        .batch_set_fields(&[(id1, updates1), (id2, updates2)])
        .await
        .unwrap();

    let t1 = store.get(id1).await.unwrap();
    let t2 = store.get(id2).await.unwrap();
    assert_eq!(t1.summary, "first");
    assert_eq!(t2.summary, "second");
}

#[tokio::test]
async fn batch_set_fields_empty_is_noop() {
    let store = TaskStore::open_memory().await.unwrap();
    store.batch_set_fields(&[]).await.unwrap();
}

#[tokio::test]
async fn batch_set_fields_rejects_disallowed_column() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = make_task(&store).await;
    let updates: &[(&str, serde_json::Value)] = &[("status", serde_json::json!("done"))];
    let err = store.batch_set_fields(&[(id, updates)]).await;
    assert!(err.is_err());
}

#[tokio::test]
async fn batch_increment_multiple_tasks() {
    let store = TaskStore::open_memory().await.unwrap();
    let id1 = make_task(&store).await;
    let id2 = make_task(&store).await;

    store
        .batch_increment(&[(id1, "attempts"), (id2, "attempts"), (id1, "attempts")])
        .await
        .unwrap();

    let t1 = store.get(id1).await.unwrap();
    let t2 = store.get(id2).await.unwrap();
    assert_eq!(t1.attempts, 2);
    assert_eq!(t2.attempts, 1);
}

#[tokio::test]
async fn batch_increment_rejects_disallowed_field() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = make_task(&store).await;
    let err = store.batch_increment(&[(id, "review_cycles_bad")]).await;
    assert!(err.is_err());
}

#[tokio::test]
async fn batch_reset_failure_counters_preserves_review_cycles() {
    let store = TaskStore::open_memory().await.unwrap();
    let id1 = make_task(&store).await;
    let id2 = make_task(&store).await;

    for id in [id1, id2] {
        store.increment(id, "attempts").await.unwrap();
        store.increment(id, "review_cycles").await.unwrap();
        store.increment(id, "ci_merge_failures").await.unwrap();
        store.increment(id, "merge_conflict_retries").await.unwrap();
        store.increment(id, "needs_review_refires").await.unwrap();
    }

    store
        .batch_reset_failure_counters(&[id1, id2])
        .await
        .unwrap();

    for id in [id1, id2] {
        let t = store.get(id).await.unwrap();
        assert_eq!(t.attempts, 1, "attempts must be preserved (monotonic)");
        assert_eq!(t.merge_conflict_retries, 0);
        assert_eq!(
            t.needs_review_refires, 0,
            "needs_review_refires must be reset"
        );
        assert_eq!(t.review_cycles, 1, "review_cycles must be preserved");
        assert_eq!(
            t.ci_merge_failures, 1,
            "ci_merge_failures must be preserved"
        );
    }
}

#[tokio::test]
async fn batch_reset_failure_counters_empty_is_noop() {
    let store = TaskStore::open_memory().await.unwrap();
    store.batch_reset_failure_counters(&[]).await.unwrap();
}

#[tokio::test]
async fn batch_mark_cleaned_sets_flag() {
    let store = TaskStore::open_memory().await.unwrap();
    let id1 = make_task(&store).await;
    let id2 = make_task(&store).await;

    store.batch_mark_cleaned(&[id1, id2]).await.unwrap();

    let t1 = store.get(id1).await.unwrap();
    let t2 = store.get(id2).await.unwrap();
    assert!(t1.worktree_cleaned);
    assert!(t2.worktree_cleaned);
}

#[tokio::test]
async fn batch_mark_cleaned_empty_is_noop() {
    let store = TaskStore::open_memory().await.unwrap();
    store.batch_mark_cleaned(&[]).await.unwrap();
}

// -----------------------------------------------------------------------
// Regression tests for #1736: SQLite OOB panics (sqlx-sqlite workers)
// -----------------------------------------------------------------------

/// Verifies that TASK_COLS_COUNT matches the number of comma-separated
/// columns in the TASK_COLS constant.  If a developer adds a column to
/// TASK_COLS but forgets to update TASK_COLS_COUNT (or vice-versa) this
/// test will catch it at compile+test time, long before a deployment.
#[test]
fn task_cols_count_matches_task_cols_string() {
    let count = super::tasks::TASK_COLS.split(',').count();
    assert_eq!(
        count,
        super::tasks::TASK_COLS_COUNT,
        "TASK_COLS has {count} comma-separated columns but TASK_COLS_COUNT = {}. \
         Update TASK_COLS_COUNT to match.",
        super::tasks::TASK_COLS_COUNT,
    );
}

/// Verifies that TASK_COLS_COUNT matches the number of columns in the
/// `tasks` table after all migrations have been applied.
///
/// This is the primary regression guard for #1736: if a new migration
/// adds a column to `tasks` without a corresponding update to TASK_COLS
/// and TASK_COLS_COUNT, this test will fail — preventing the OOB panic
/// from reaching production.
#[tokio::test]
async fn task_cols_count_matches_schema() {
    use sqlx::Row as _;

    let store = TaskStore::open_memory().await.unwrap();

    let row = sqlx::query("SELECT COUNT(*) as cnt FROM pragma_table_info('tasks')")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let schema_count: i64 = row.try_get("cnt").unwrap();

    assert_eq!(
        schema_count as usize,
        super::tasks::TASK_COLS_COUNT,
        "tasks table has {schema_count} columns after migrations but \
         TASK_COLS_COUNT = {}. Add the new column(s) to TASK_COLS and \
         increment TASK_COLS_COUNT.",
        super::tasks::TASK_COLS_COUNT,
    );
}

/// End-to-end regression test for #1736: creates a task row and reads it
/// back through the full `row_to_task` deserialization path.  If TASK_COLS
/// references a column that doesn't exist in the schema, or if the column
/// index mapping is wrong, this test will panic (or fail) rather than
/// silently crashing a sqlx-sqlite worker thread in production.
#[tokio::test]
async fn task_row_deserialization_no_oob_panic() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "OOB regression".to_string(),
            body: "Regression test for #1736".to_string(),
            source: "test".to_string(),
            source_id: "oob-test".to_string(),
            author: "".to_string(),
            url: "".to_string(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // This exercises the full SELECT TASK_COLS + row_to_task path.
    let task = store.get(id).await.unwrap();
    assert_eq!(task.title, "OOB regression");
    assert_eq!(task.no_code_reroutes, 0);
    assert_eq!(task.auto_unblock_count, 0);
    assert_eq!(task.ci_recovery_count, 0);
}
