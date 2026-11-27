use super::super::*;
use crate::engine::runner::TaskRunner;
use std::sync::Arc;

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
async fn helper_get_token_usage_negative_tokens_clamp_to_zero() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let id = store
        .create(&NewTask {
            external_id: Some("701".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Negative token test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    store
        .set_fields(
            id,
            &[
                ("input_tokens", serde_json::json!(-42)),
                ("output_tokens", serde_json::json!(8)),
            ],
        )
        .await
        .unwrap();

    let opt_store = Some(store);
    let usage = get_token_usage(&opt_store, "owner/repo", "701").await;
    assert_eq!(usage.input_tokens, 0);
    assert_eq!(usage.output_tokens, 8);
    assert_eq!(usage.total_tokens(), 8);
}

#[tokio::test]
async fn helper_get_reported_cost_from_store() {
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
        .set_fields(id, &[("total_cost_usd", serde_json::json!(0.15))])
        .await
        .unwrap();

    let opt_store = Some(store);
    let cost = get_reported_cost(&opt_store, "owner/repo", "71").await;
    assert_eq!(cost, Some(0.15));
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

    let opt_store = Some(Arc::clone(&store));
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

    let opt_store = Some(Arc::clone(&store));
    let _ = store_set_by_id(
        opt_store.as_ref(),
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

    let opt_store = Some(Arc::clone(&store));
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
async fn helper_store_reset_failure_counters_preserves_review_cycles_and_merge_conflict_retries() {
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

    let opt_store = Some(Arc::clone(&store));
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
    assert_eq!(
        task.review_invocations, 1,
        "review_invocations must be preserved (monotonic — same reason as attempts)"
    );
    assert_eq!(task.attempts, 1, "attempts must be preserved (monotonic)");
    assert_eq!(
        task.merge_conflict_retries, 1,
        "merge_conflict_retries must be preserved"
    );
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

    let opt_store = Some(Arc::clone(&store));
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
async fn helper_get_recent_memory_returns_empty_without_store() {
    let store: Option<Arc<TaskStore>> = None;
    let memory = get_recent_memory(&store, "owner/repo", "any", 10).await;
    assert!(memory.is_empty(), "{memory:?}");
}

#[tokio::test]
async fn task_runner_with_store() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    // Just verify it compiles and the builder works
    let _runner = TaskRunner::new("owner/repo".to_string()).with_store(store);
}

#[tokio::test]
async fn helper_get_token_usage_result_without_store_returns_err() {
    let store: Option<Arc<TaskStore>> = None;
    let res = get_token_usage_result(&store, "owner/repo", "any").await;
    assert!(res.is_err());
}

#[tokio::test]
async fn helper_get_reported_cost_result_without_store_returns_err() {
    let store: Option<Arc<TaskStore>> = None;
    let res = get_reported_cost_result(&store, "owner/repo", "any").await;
    assert!(res.is_err());
}

#[tokio::test]
async fn helper_get_recent_memory_result_without_store_returns_err() {
    let store: Option<Arc<TaskStore>> = None;
    let res = get_recent_memory_result(&store, "owner/repo", "any", 10).await;
    assert!(res.is_err());
}

#[tokio::test]
async fn helper_get_recent_memory_result_not_found_returns_err() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    store
        .create(&NewTask {
            external_id: Some("not-this-task".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Different task".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let opt_store = Some(store);
    let res =
        get_recent_memory_result(&opt_store, "owner/repo", "nonexistent-task-12345", 10).await;
    assert!(
        res.is_err(),
        "get_recent_memory_result should return error for missing task, not Ok(None)"
    );
}

#[tokio::test]
async fn helper_get_result_without_store_uses_unavailable_message() {
    let store: Option<Arc<TaskStore>> = None;
    let token = get_token_usage_result(&store, "owner/repo", "any").await;
    let cost = get_reported_cost_result(&store, "owner/repo", "any").await;
    let memory = get_recent_memory_result(&store, "owner/repo", "any", 10).await;
    assert_eq!(token.unwrap_err().to_string(), "task store unavailable");
    assert_eq!(cost.unwrap_err().to_string(), "task store unavailable");
    assert_eq!(memory.unwrap_err().to_string(), "task store unavailable");
}

#[tokio::test]
async fn helper_get_result_missing_task_uses_repo_task_message() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let opt_store = Some(store);
    let token = get_token_usage_result(&opt_store, "owner/repo", "missing-94").await;
    let cost = get_reported_cost_result(&opt_store, "owner/repo", "missing-94").await;
    let memory = get_recent_memory_result(&opt_store, "owner/repo", "missing-94", 10).await;
    let expected = "task owner/repo/missing-94 not found in store";
    assert_eq!(token.unwrap_err().to_string(), expected);
    assert_eq!(cost.unwrap_err().to_string(), expected);
    assert_eq!(memory.unwrap_err().to_string(), expected);
}

#[tokio::test]
async fn helper_store_touch_updated_at_advances_timestamp() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let id = store
        .create(&NewTask {
            external_id: Some("94".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Touch test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    let opt_store = Some(Arc::clone(&store));
    store_touch_updated_at(&opt_store, "owner/repo", "94").await;

    let task = store.get(id).await.unwrap();
    assert_ne!(task.updated_at, "2020-01-01T00:00:00Z");
}

#[tokio::test]
async fn helper_store_touch_updated_at_noop_without_store_or_task() {
    let none: Option<Arc<TaskStore>> = None;
    store_touch_updated_at(&none, "owner/repo", "no-task").await;

    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let opt_store = Some(store);
    store_touch_updated_at(&opt_store, "owner/repo", "missing-94").await;
}

#[tokio::test]
async fn helper_store_log_activity_writes_a_row() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let id = store
        .create(&NewTask {
            external_id: Some("95".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Activity test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let opt_store = Some(Arc::clone(&store));
    store_log_activity(
        &opt_store,
        "owner/repo",
        "95",
        "session_exit",
        Some("in_progress"),
        Some("needs_review"),
        Some("claude"),
        None,
        None,
    )
    .await;

    let events = store.get_activity(id, None).await.unwrap();
    let logged = events
        .iter()
        .find(|e| e.event_type == "session_exit")
        .expect("session_exit row");
    assert_eq!(logged.from_status.as_deref(), Some("in_progress"));
    assert_eq!(logged.to_status.as_deref(), Some("needs_review"));
    assert_eq!(logged.agent.as_deref(), Some("claude"));
    assert_eq!(logged.model, None);
}

#[tokio::test]
async fn helper_store_log_activity_noop_without_store_or_task() {
    let none: Option<Arc<TaskStore>> = None;
    store_log_activity(
        &none,
        "owner/repo",
        "no-task",
        "session_exit",
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let opt_store = Some(store);
    store_log_activity(
        &opt_store,
        "owner/repo",
        "missing-95",
        "session_exit",
        None,
        None,
        None,
        None,
        None,
    )
    .await;
}
