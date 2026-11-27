#![expect(
    clippy::float_cmp,
    reason = "the prices and costs are stored constants, so they compare exactly"
)]

mod cli_ref;
mod control;
mod fields;
mod helpers;
mod metrics;
mod runs;
use super::*;
use crate::backends::ExternalId;
use crate::backends::ExternalTask;
use sha2::Digest;
use sqlx::ConnectOptions;
use sqlx::Row;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::process;

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
            author: String::new(),
            url: String::new(),
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
async fn reset_to_new_clears_branch_and_worktree() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create_internal("owner/repo", "reset", "", "test", "reset-clear", None)
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Routed).await.unwrap();
    store
        .set_fields(
            id,
            &[
                ("branch", serde_json::json!("gh-issue-1-x")),
                ("worktree", serde_json::json!("/tmp/wt")),
            ],
        )
        .await
        .unwrap();

    store.reset_to_new(id).await.unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::New);
    assert!(task.branch.is_empty(), "branch is cleared");
    assert!(task.worktree.is_empty(), "worktree is cleared");
}

#[tokio::test]
async fn reset_to_new_keep_branch_leaves_the_branch_name() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create_internal("owner/repo", "reset", "", "test", "reset-keep", None)
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Routed).await.unwrap();
    store
        .set_fields(
            id,
            &[
                ("branch", serde_json::json!("gh-issue-1-x")),
                ("worktree", serde_json::json!("/tmp/wt")),
            ],
        )
        .await
        .unwrap();

    store.reset_to_new_keep_branch(id).await.unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::New);
    assert_eq!(task.branch, "gh-issue-1-x");
    assert!(task.worktree.is_empty(), "worktree is cleared");
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
                body: String::new(),
                source: "cron".to_string(),
                source_id: format!("job-{i}"),
                author: String::new(),
                url: String::new(),
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
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo-b".to_string(),
            origin: "internal".to_string(),
            title: "Task B".to_string(),
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
            body: String::new(),
            source: "webhook".to_string(),
            source_id: String::new(),
            author: "user".to_string(),
            url: String::new(),
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

    let child_id = store
        .create(&NewTask {
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Child".to_string(),
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
        .set_fields(child_id, &[("parent_id", serde_json::json!(parent_id))])
        .await
        .unwrap();

    let child = store.get(child_id).await.unwrap();
    assert_eq!(child.parent_id, Some(parent_id));
}

#[tokio::test]
async fn upsert_external_sets_parent_id_from_parent_label() {
    let store = TaskStore::open_memory().await.unwrap();

    let parent_id = store
        .create_internal("owner/repo", "Parent", "", "manual", "", None)
        .await
        .unwrap();
    let labels = vec![format!("parent:internal:{parent_id}")];

    let child_id = store
        .upsert_external(&UpsertExternal {
            repo: "owner/repo",
            ext_id: "42",
            title: "Child",
            body: "",
            author: "bot",
            url: "https://github.com/owner/repo/issues/42",
            labels: &labels,
            origin: "github",
        })
        .await
        .unwrap();

    let child = store.get(child_id).await.unwrap();
    assert_eq!(child.parent_id, Some(parent_id));

    let children = store.list_children("owner/repo", parent_id).await.unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].external_id.as_deref(), Some("42"));
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
            body: String::new(),
            source: "webhook".to_string(),
            source_id: String::new(),
            author: "user".to_string(),
            url: String::new(),
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

    let id2 = store
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
    store.update_status(id2, TaskStatus::Done).await.unwrap();

    let routable = store.list_routable("owner/repo").await.unwrap();
    assert_eq!(routable.len(), 1);
    assert_eq!(routable[0].id, id1);
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

    let task = store.get(id).await.unwrap();
    assert_ne!(task.created_at, "");
    assert_ne!(task.updated_at, "");
    assert!(task.created_at.ends_with('Z'), "should be UTC");
    assert!(task.created_at.contains('T'), "should be RFC3339");
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
            body: String::new(),
            source: "cron".to_string(),
            source_id: "daily".to_string(),
            author: String::new(),
            url: String::new(),
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
    assert!(task.memory.is_empty(), "{:?}", task.memory);
    assert!(task.delegations.is_empty(), "{:?}", task.delegations);
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
async fn ensure_external_task_upserts() {
    let store = TaskStore::open_memory().await.unwrap();

    let ext = ExternalTask {
        id: ExternalId("42".to_string()),
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

// ── prune_old_runs ──────────────────────────────────────────────────────

// ── complete_run with full lifecycle ─────────────────────────────────────

// ── cost_summary edge cases ─────────────────────────────────────────────

// ── status_counts edge cases ────────────────────────────────────────────

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

    let ext1 = ExternalTask {
        id: ExternalId("42".to_string()),
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
    let ext2 = ExternalTask {
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

// ── full run lifecycle: start → complete → query ────────────────────────

// ── set_fields rejects unknown columns ──────────────────────────────────

// ── append_memory + recent_memory ───────────────────────────────────────

// ── mark_cleaned + list_cleanable ───────────────────────────────────────

// ── store_tokens accumulates ────────────────────────────────────────────

// ── increment field ─────────────────────────────────────────────────────

// ── with_store on TaskRunner ─────────────────────────────────────────

#[tokio::test]
async fn ensure_external_task_creates_then_updates() {
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

// ── resolve_task_id_globally ────────────────────────────────────────────

// ── resolve_cli_id ──────────────────────────────────────────────────────

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

// ── Rate Limits ─────────────────────────────────────────────────

// ── Self-improvement counter ─────────────────────────────────────

// ── Slow tasks & error distribution ─────────────────────────────

// ── cost_summary static queries ──────────────────────────────────────

// ── has_external_tasks ───────────────────────────────────────────

#[tokio::test]
async fn has_external_tasks_returns_false_for_empty_repo() {
    let store = TaskStore::open_memory().await.unwrap();
    assert!(!store.has_external_tasks("owner/repo").await.unwrap());
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
    assert!(store.has_external_tasks("owner/repo").await.unwrap());
    // Different repo should still be false
    assert!(!store.has_external_tasks("other/repo").await.unwrap());
}

#[tokio::test]
async fn has_external_tasks_ignores_internal_rows() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .create_internal("owner/repo", "task", "body", "manual", "", None)
        .await
        .unwrap();

    assert!(!store.has_external_tasks("owner/repo").await.unwrap());

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

    assert!(store.has_external_tasks("owner/repo").await.unwrap());
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

// ── get_last_run ────────────────────────────────────────────────

// ── prune_old_runs ──────────────────────────────────────────────

// ── increment named column alias ────────────────────────────────

// ── get_cost_summary_by_repo ───────────────────────────────────

// ── parse_since_duration ──────────────────────────────────────────────────────

// ── list_control_messages with since ─────────────────────────────────────────

// ── search_control_messages with since ───────────────────────────────────────

/// The full migration set must apply cleanly on an empty database.
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
    let tmp = env::temp_dir().join(format!("orch-migration-test-{}.db", process::id()));

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
    let _ = fs::remove_file(&tmp);
    let _ = fs::remove_file(tmp.with_extension("db-shm"));
    let _ = fs::remove_file(tmp.with_extension("db-wal"));
}

/**
 * Guard against editing an already-applied migration. `migrations/` files are immutable once
 * merged: SQLx stores a checksum per applied migration and refuses to start on a mismatch. Tests
 * that open a fresh DB cannot catch an edit, so checksums are committed in
 * `migrations/checksums.txt` and recomputed here. A mismatch means a file was edited; a missing
 * entry means a new migration was added without appending its line
 **/
#[tokio::test]
async fn migrations_match_committed_checksums() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let committed =
        fs::read_to_string(dir.join("checksums.txt")).expect("migrations/checksums.txt must exist");
    let mut expected: HashMap<String, String> = committed
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let (sum, name) = l.split_once("  ").expect("checksum line: <sha256>  <file>");
            (name.trim().to_string(), sum.to_string())
        })
        .collect();

    let mut actual_count = 0;
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("sql") {
            continue;
        }
        actual_count += 1;
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let sum = format!("{:x}", sha2::Sha256::digest(fs::read(&path).unwrap()));
        let committed_sum = expected.remove(&name).unwrap_or_else(|| {
            panic!(
                "{name} has no entry in checksums.txt, append its checksum when adding a migration"
            )
        });
        assert_eq!(
            sum, committed_sum,
            "{name} was modified after its checksum was committed, add a new migration instead"
        );
    }

    assert_eq!(
        expected.len(),
        0,
        "checksums.txt lists files that no longer exist: {expected:?}"
    );
    assert!(actual_count > 0, "no migration files found");
}

// ── Recent rate limit counts (health check query) ───────────────

// ---------------------------------------------------------------------------
// Batch methods
// ---------------------------------------------------------------------------

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
    let store = TaskStore::open_memory().await.unwrap();

    let row = sqlx::query("SELECT COUNT(*) as cnt FROM pragma_table_info('tasks')")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let schema_count: i64 = row.try_get("cnt").unwrap();

    assert_eq!(
        usize::try_from(schema_count).unwrap(),
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
            author: String::new(),
            url: String::new(),
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

// Tests for the *_result variants — verify they propagate errors instead of
// returning defaults, so callers can fail-safe on store read errors.

#[tokio::test]
async fn move_repo_keeps_tasks_and_dependencies() {
    let store = TaskStore::open_memory().await.unwrap();
    let mk = |n: &str| NewTask {
        external_id: Some(n.to_string()),
        repo: "a/x".to_string(),
        origin: "github".to_string(),
        title: format!("t{n}"),
        ..Default::default()
    };
    let one = store.create(&mk("1")).await.unwrap();
    let two = store.create(&mk("2")).await.unwrap();
    store
        .replace_dependencies(
            two,
            &[NewDependency {
                repo: "a/x".to_string(),
                number: 1,
                source: DepSource::Cli,
            }],
        )
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET branch = 'br' WHERE id = ?")
        .bind(one)
        .execute(&store.pool)
        .await
        .unwrap();
    let dropped = [
        "dep_blocker:a/x#1",
        "dep_blocker_read_at:a/x#1",
        "review_batch_fail:a/x",
    ];
    let renamed = [
        (
            "dep_blocker_notice:7:a/x#1:blocked",
            "dep_blocker_notice:7:b/x#1:blocked",
        ),
        ("dep_cycle_comment:a/x#1:sig", "dep_cycle_comment:b/x#1:sig"),
        ("review_approved_sha:a/x#2#9", "review_approved_sha:b/x#2#9"),
        ("issues_last_ingested:a/x", "issues_last_ingested:b/x"),
        (
            "review_ci_wait_since:a/x#1#abc",
            "review_ci_wait_since:b/x#1#abc",
        ),
        (
            "ci_bootstrap_wait_since:a/x#1#abc",
            "ci_bootstrap_wait_since:b/x#1#abc",
        ),
        (
            "ci_bootstrap_no_checks_comment:a/x#1#abc",
            "ci_bootstrap_no_checks_comment:b/x#1#abc",
        ),
        (
            "ci_bootstrap_never_run_comment:a/x#1",
            "ci_bootstrap_never_run_comment:b/x#1",
        ),
    ];
    // a repo whose name starts with the moved one must keep its keys
    let untouched = ["dep_blocker:a/x2#1", "issues_last_ingested:a/x2"];
    for key in dropped
        .iter()
        .chain(renamed.iter().map(|(from, _)| from))
        .chain(untouched.iter())
    {
        store.kv_set(key, "1").await.unwrap();
    }

    assert_eq!(store.move_repo("a/x", "b/x").await.unwrap(), 2);
    for key in dropped.iter().chain(renamed.iter().map(|(from, _)| from)) {
        assert_eq!(store.kv_get(key).await.unwrap(), None, "{key} left behind");
    }
    for (_, to) in renamed {
        assert_eq!(
            store.kv_get(to).await.unwrap().as_deref(),
            Some("1"),
            "{to} missing"
        );
    }
    for key in untouched {
        assert_eq!(
            store.kv_get(key).await.unwrap().as_deref(),
            Some("1"),
            "{key} touched"
        );
    }
    let t = store.get(one).await.unwrap();
    assert_eq!((t.repo.as_str(), t.id), ("b/x", one));
    assert_eq!(t.branch, "br");
    let deps = store.list_dependencies(two).await.unwrap();
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0].depends_on_repo, "b/x");
    assert_eq!(deps[0].depends_on_task_id, Some(one));
}

#[tokio::test]
async fn move_repo_refuses_when_target_has_tasks() {
    let store = TaskStore::open_memory().await.unwrap();
    for repo in ["a/x", "b/x"] {
        store
            .create(&NewTask {
                repo: repo.to_string(),
                origin: "internal".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    assert!(store.move_repo("a/x", "b/x").await.is_err());
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks WHERE repo = 'a/x'")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

async fn locking_connection(path: &Path) -> sqlx::SqliteConnection {
    SqliteConnectOptions::new()
        .filename(path)
        .busy_timeout(Duration::ZERO)
        .connect()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_write_lock_conflict_is_recognized() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("orch.db");
    let store = TaskStore::open(&path).await.unwrap();
    let held = store.begin_write().await.unwrap();

    let mut other = locking_connection(&path).await;
    let err = sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut other)
        .await
        .unwrap_err();
    drop(held);

    assert!(is_write_lock_error(&anyhow::Error::from(err)));
    assert!(!is_write_lock_error(&anyhow::anyhow!("some other failure")));
}

#[tokio::test]
async fn a_store_write_waits_for_another_connection_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("orch.db");
    let store = TaskStore::open(&path).await.unwrap();
    let id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "locked".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let mut other = locking_connection(&path).await;
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut other)
        .await
        .unwrap();
    let release = tokio::spawn(async move {
        time::sleep(Duration::from_millis(300)).await;
        sqlx::query("COMMIT").execute(&mut other).await.unwrap();
    });

    store.update_status(id, TaskStatus::Routed).await.unwrap();
    release.await.unwrap();
    assert_eq!(store.get(id).await.unwrap().status, TaskStatus::Routed);
}

/// The bundled SQLite must stay at 3.54.0 or later, older releases have known memory-safety CVEs
#[tokio::test]
async fn bundled_sqlite_is_at_least_3_54() {
    let store = TaskStore::open_memory().await.unwrap();
    let version: String = sqlx::query_scalar("select sqlite_version()")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let parts: Vec<u32> = version.split('.').map(|p| p.parse().unwrap()).collect();
    assert!(
        (parts[0], parts[1]) >= (3, 54),
        "bundled SQLite is {version}, need 3.54.0 or later"
    );
}

#[tokio::test]
async fn job_state_is_absent_until_upserted() {
    let store = TaskStore::open_memory().await.unwrap();

    assert!(
        store
            .get_job_state("owner/repo", "daily")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .list_job_states("owner/repo")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn job_state_with_empty_fields_round_trips() {
    let store = TaskStore::open_memory().await.unwrap();
    let state = JobState {
        repo: "owner/repo".to_string(),
        job_id: "daily".to_string(),
        last_run: None,
        last_task_status: None,
        active_task_id: None,
    };

    store.upsert_job_state(&state).await.unwrap();

    assert_eq!(
        store.get_job_state("owner/repo", "daily").await.unwrap(),
        Some(state.clone())
    );
    assert_eq!(
        store.list_job_states("owner/repo").await.unwrap(),
        vec![state]
    );
    assert!(
        store
            .list_job_states("other/repo")
            .await
            .unwrap()
            .is_empty()
    );
}
