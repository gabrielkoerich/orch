use super::super::*;

#[tokio::test]
async fn increment_and_reset_counters() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
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
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
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
        err.to_string().contains("out of range") || err.to_string().contains("decod"),
        "unexpected error: {err:#}"
    );
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
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
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
async fn set_fields_redacts_last_error() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    store
        .set_fields(
            id,
            &[(
                "last_error",
                serde_json::json!(
                    "push failed: https://x-access-token:ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij@github.com/owner/repo.git"
                ),
            )],
        )
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert!(!task.last_error.contains("ghp_"));
    assert!(task.last_error.contains("[REDACTED:github-pat]"));
}

#[tokio::test]
async fn set_block_reason_redacts_sensitive_git_error() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = make_task(&store).await;

    store
        .set_block_reason(
            id,
            Some(
                "push failed: https://x-access-token:ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij@github.com/owner/repo.git",
            ),
        )
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    let block_reason = task.block_reason.expect("block_reason should be set");
    assert!(!block_reason.contains("ghp_"));
    assert!(block_reason.contains("[REDACTED:github-pat]"));
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
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
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
async fn list_cleanable() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Done task".to_string(),
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
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

#[tokio::test]
async fn set_fields_empty_is_noop() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
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
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
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
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
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
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
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
        })
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.agent, Some("codex".to_string()));
    assert_eq!(task.model, Some("gpt-5.2".to_string()));
    assert_eq!(task.complexity, "complex");
    assert_eq!(task.estimate, 8);
    assert_eq!(task.route_reason, "needs deep analysis");
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
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
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
async fn reset_failure_counters_preserves_review_cycles_and_merge_conflict_retries() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // Simulate counters after a review cycle: review_cycles=1,
    // ci_merge_failures=2, merge_conflict_retries=1, plus transient failures.
    store.increment(id, "review_cycles").await.unwrap();
    store.increment(id, "ci_merge_failures").await.unwrap();
    store.increment(id, "ci_merge_failures").await.unwrap();
    store.increment(id, "attempts").await.unwrap();
    store.increment(id, "review_invocations").await.unwrap();
    store.increment(id, "review_agent_failures").await.unwrap();
    store.increment(id, "merge_conflict_retries").await.unwrap();
    store.increment(id, "network_retries").await.unwrap();

    // reset_failure_counters must zero transient counters but preserve
    // review_cycles, ci_merge_failures, merge_conflict_retries, attempts, and
    // review_invocations. Both attempts and review_invocations must be monotonic
    // so that (task_id, attempt, run_type) keys in task_runs remain unique.
    store.reset_failure_counters(id).await.unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.review_cycles, 1, "review_cycles must be preserved");
    assert_eq!(
        task.ci_merge_failures, 2,
        "ci_merge_failures must be preserved"
    );
    assert_eq!(
        task.merge_conflict_retries, 1,
        "merge_conflict_retries must be preserved"
    );
    assert_eq!(task.attempts, 1, "attempts must be preserved (monotonic)");
    assert_eq!(
        task.review_invocations, 1,
        "review_invocations must be preserved (monotonic — same reason as attempts)"
    );
    assert_eq!(task.review_agent_failures, 0);
    assert_eq!(task.network_retries, 0);
}

/// Regression test for issue #2392.
///
/// `reset_failure_counters` zeros `no_code_reroutes`. This is why
/// `ReviewDecision::Rerouted` must NOT call `reset_failure_counters` — the
/// `Skipped` path did call it, causing the counter to reset to 0 on every cycle
/// so the `max_reroute_attempts` limit was never reached.
#[tokio::test]
async fn reset_failure_counters_resets_no_code_reroutes() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "no-code-reroute regression".to_string(),
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    store.increment(id, "no_code_reroutes").await.unwrap();
    store.increment(id, "no_code_reroutes").await.unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(task.no_code_reroutes, 2);

    // Calling reset_failure_counters (the Skipped path) zeros the counter —
    // this is the original bug: the counter could never reach max_reroute_attempts.
    store.reset_failure_counters(id).await.unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.no_code_reroutes, 0,
        "reset_failure_counters must zero no_code_reroutes (bug: Skipped path called this)"
    );
}

/// Regression test for issue #2392.
///
/// When `ReviewDecision::Rerouted` is returned the subscriber must NOT call
/// `reset_failure_counters`, so `no_code_reroutes` accumulates across cycles
/// and the `max_reroute_attempts` safety limit is reachable.
#[tokio::test]
async fn no_code_reroutes_accumulates_when_reset_not_called() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "no-code-reroute accumulation regression".to_string(),
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // Simulate 3 consecutive Rerouted cycles: increment only, no reset.
    for _ in 0..3 {
        store.increment(id, "no_code_reroutes").await.unwrap();
        // ReviewDecision::Rerouted path: reset_failure_counters is NOT called here.
    }

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.no_code_reroutes, 3,
        "no_code_reroutes must accumulate across Rerouted cycles so max_reroute_attempts is enforced"
    );
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
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
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
async fn concurrent_increments() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
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
async fn set_fields_persists_review_invocations() {
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
        .set_fields(id, &[("review_invocations", serde_json::json!(3))])
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.review_invocations, 3);
}

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
}

#[tokio::test]
async fn set_estimate_if_zero_populates_estimate() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: Some("100".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Estimate me".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    // estimate starts at 0
    let task = store.get(id).await.unwrap();
    assert_eq!(task.estimate, 0);

    // set_estimate_if_zero should return true and update when estimate is 0
    let updated = store.set_estimate_if_zero(id, 5).await.unwrap();
    assert!(updated, "should return true when estimate was 0");

    let task = store.get(id).await.unwrap();
    assert_eq!(task.estimate, 5, "estimate should be updated to 5");
}

#[tokio::test]
async fn set_estimate_if_zero_does_not_overwrite() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: Some("101".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Already estimated".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    // Set estimate via store_route (simulates router assigning an estimate).
    store
        .store_route(&StoreRoute {
            id,
            agent: "claude",
            model: Some("sonnet"),
            complexity: "medium",
            estimate: 8,
            reason: "complex refactor",
            profile: "{}",
        })
        .await
        .unwrap();

    // set_estimate_if_zero should return false when estimate is already set
    let updated = store.set_estimate_if_zero(id, 3).await.unwrap();
    assert!(
        !updated,
        "should return false when estimate was already non-zero"
    );

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.estimate, 8,
        "estimate should remain 8, not overwritten to 3"
    );
}

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

#[tokio::test]
async fn set_fields_with_numeric_bool_null_types() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: Some("99".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Type test".to_string(),
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

    // Set a numeric field
    store
        .set_fields(id, &[("pr_number", serde_json::json!(42))])
        .await
        .unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(task.pr_number, Some(42));

    // Set a boolean field
    store
        .set_fields(id, &[("worktree_cleaned", serde_json::json!(true))])
        .await
        .unwrap();
    let task = store.get(id).await.unwrap();
    assert!(task.worktree_cleaned);

    // Set a field to null (empty string)
    store
        .set_fields(id, &[("summary", serde_json::Value::Null)])
        .await
        .unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(task.summary, "");
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
async fn set_fields_multiple_fields_at_once() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: Some("1".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Multi-field".to_string(),
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
async fn set_fields_silent_does_not_bump_updated_at() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: Some("1".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Silent update".to_string(),
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

    // Backdate updated_at to a known stale timestamp.
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(&store.pool)
        .await
        .unwrap();

    store
        .set_fields_silent(id, &[("agent", serde_json::json!("claude"))])
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.agent, Some("claude".to_string()));
    assert_eq!(
        task.updated_at, "2020-01-01T00:00:00Z",
        "set_fields_silent must not touch updated_at"
    );
}

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

async fn make_task(store: &TaskStore) -> i64 {
    store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Batch test".to_string(),
            body: String::new(),
            source: "manual".to_string(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
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
async fn batch_set_fields_redacts_last_error() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = make_task(&store).await;
    let updates: &[(&str, serde_json::Value)] = &[(
        "last_error",
        serde_json::json!(
            "push failed: https://x-access-token:ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij@github.com/owner/repo.git"
        ),
    )];

    store.batch_set_fields(&[(id, updates)]).await.unwrap();

    let task = store.get(id).await.unwrap();
    assert!(!task.last_error.contains("ghp_"));
    assert!(task.last_error.contains("[REDACTED:github-pat]"));
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
async fn batch_reset_failure_counters_preserves_review_cycles_and_merge_conflict_retries() {
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
        assert_eq!(
            t.merge_conflict_retries, 1,
            "merge_conflict_retries must be preserved"
        );
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
