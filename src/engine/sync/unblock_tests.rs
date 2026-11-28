use super::tests::IngestMockBackend;
use super::tests::router_with_healthy_review_agent;
use super::tests::sweep_repos;
use super::unblock::{
    ci_failure_unblock_cooldown_elapsed, is_ci_failure_block, try_unblock_ci_failure_task,
};
use super::*;
use crate::engine::ci::{
    CI_FAILURE_AUTO_MERGE_STAGE, CI_FAILURE_BEFORE_REVIEW_STAGE, ci_failure_limit_reason,
};
use crate::engine::router::Router;
use crate::engine::router::RouterConfig;
use crate::github::token::ENV_LOCK;
use crate::store::{RunTokenUsage, TaskStore};
use serial_test::serial;
use std::env;
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Regression test for #3499: a task blocked with the rebroadcast-escalation
/// reason after surviving one review round (`review_cycles > 0`) must be
/// recovered back to NeedsReview once review agents are routable again.
/// Previously the `review_cycles != 0` filter excluded these tasks, stranding
/// them permanently even when agents recovered.
#[serial(cooldown_state)]
#[tokio::test]
async fn auto_recover_rebroadcast_blocked_task_after_review_cycle() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let router = router_with_healthy_review_agent();

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "511",
            title: "Rebroadcast blocked after review",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Blocked).await.unwrap();
    store
        .set_block_reason(
            id,
            Some("review agent rebroadcast escalated after repeated retries"),
        )
        .await
        .unwrap();
    store
        .set_fields(id, &[("review_cycles", serde_json::json!(1))])
        .await
        .unwrap();
    store
        .set_fields(id, &[("needs_review_refires", serde_json::json!(6))])
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    auto_recover_rebroadcast_blocked_tasks_global(&router, &task_manager, &store, &sweep_repos())
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::NeedsReview,
        "blocked rebroadcast task must be recovered to needs_review"
    );
    assert!(
        task.block_reason.is_none(),
        "block_reason must be cleared on recovery"
    );
    assert_eq!(
        task.needs_review_refires, 0,
        "needs_review_refires must be reset to 0 on recovery"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn auto_unblock_routes_recoverable_failure() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "77",
            title: "Blocked task",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Blocked).await.unwrap();
    store
        .set_fields(
            id,
            &[
                ("agent", serde_json::json!("codex")),
                ("model", serde_json::json!("gpt-5")),
            ],
        )
        .await
        .unwrap();

    let run_id = store
        .start_run(&crate::store::StartRun {
            task_id: id,
            attempt: 1,
            run_type: "agent",
            agent: "codex",
            model: "gpt-5",
            command: "codex --model gpt-5",
            prompt: "do thing",
        })
        .await
        .unwrap();
    store
        .complete_run(&crate::store::CompleteRun {
            run_id,
            exit_code: Some(1),
            stdout: "",
            stderr: "",
            parsed: "",
            outcome: "rate_limit",
            error: "rate limit exceeded",
            tokens: RunTokenUsage::default(),
        })
        .await
        .unwrap();

    auto_unblock_blocked_tasks(
        "owner/repo",
        &task_manager,
        &store,
        &dispatching,
        &Arc::new(DashSet::new()),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::New);
    assert!(task.agent.is_none());
    assert!(task.model.is_none());
    assert_eq!(task.auto_unblock_count, 1);
    assert_ne!(task.auto_unblock_last_at, "");
    assert_eq!(task.auto_unblock_last_reason, "RateLimit");
}

#[serial(cooldown_state)]
#[tokio::test]
async fn auto_unblock_skips_manual_block() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "88",
            title: "Manually blocked",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Blocked).await.unwrap();
    store
        .set_block_reason(id, Some("waiting on input"))
        .await
        .unwrap();

    let run_id = store
        .start_run(&crate::store::StartRun {
            task_id: id,
            attempt: 1,
            run_type: "agent",
            agent: "codex",
            model: "gpt-5",
            command: "codex --model gpt-5",
            prompt: "do thing",
        })
        .await
        .unwrap();
    store
        .complete_run(&crate::store::CompleteRun {
            run_id,
            exit_code: Some(1),
            stdout: "",
            stderr: "",
            parsed: "",
            outcome: "rate_limit",
            error: "rate limit exceeded",
            tokens: RunTokenUsage::default(),
        })
        .await
        .unwrap();

    auto_unblock_blocked_tasks(
        "owner/repo",
        &task_manager,
        &store,
        &dispatching,
        &Arc::new(DashSet::new()),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(task.auto_unblock_count, 0);
}

#[serial(cooldown_state)]
#[tokio::test]
async fn auto_unblock_routes_task_blocked_by_model_unavailable() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "1202",
            title: "Task blocked by model unavailable",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Blocked).await.unwrap();

    let run_id = store
        .start_run(&crate::store::StartRun {
            task_id: id,
            attempt: 1,
            run_type: "agent",
            agent: "opencode",
            model: "anthropic/claude-sonnet-4-6",
            command: "opencode",
            prompt: "fix the bug",
        })
        .await
        .unwrap();
    store
        .complete_run(&crate::store::CompleteRun {
            run_id,
            exit_code: Some(1),
            stdout: "",
            stderr: "",
            parsed: "",
            outcome: "error",
            error: "model unavailable (anthropic/claude-sonnet-4-6): Model not found: anthropic/claude-sonnet-4-6",
            tokens: RunTokenUsage::default(),
        })
        .await
        .unwrap();

    auto_unblock_blocked_tasks(
        "owner/repo",
        &task_manager,
        &store,
        &dispatching,
        &Arc::new(DashSet::new()),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::New,
        "task blocked by model unavailable must be auto-unblocked and re-routed"
    );
    assert_eq!(task.auto_unblock_count, 1);
    assert_eq!(task.auto_unblock_last_reason, "ModelUnavailable");
}

#[serial(cooldown_state)]
#[tokio::test]
async fn auto_unblock_resets_count_when_reason_changes() {
    cooldown::reset_global_state().await;
    // Regression test for #1227: when a task is blocked for a different failure
    // reason than the last auto-unblock, the counter should reset to 0 (immediate
    // retry) instead of accumulating across unrelated failures.
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "1227",
            title: "Task with different failure reason",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Blocked).await.unwrap();
    // Simulate: task was previously auto-unblocked for ModelUnavailable (count=1)
    store
        .set_fields(
            id,
            &[
                ("auto_unblock_count", serde_json::json!(1)),
                (
                    "auto_unblock_last_reason",
                    serde_json::json!("ModelUnavailable"),
                ),
            ],
        )
        .await
        .unwrap();

    // Add a run with a DIFFERENT failure: RateLimit
    let run_id = store
        .start_run(&crate::store::StartRun {
            task_id: id,
            attempt: 2,
            run_type: "agent",
            agent: "claude",
            model: "opus",
            command: "claude",
            prompt: "fix the bug",
        })
        .await
        .unwrap();
    store
        .complete_run(&crate::store::CompleteRun {
            run_id,
            exit_code: Some(1),
            stdout: "",
            stderr: "",
            parsed: "",
            outcome: "error",
            error: "rate limit reached for model opus",
            tokens: RunTokenUsage::default(),
        })
        .await
        .unwrap();

    auto_unblock_blocked_tasks(
        "owner/repo",
        &task_manager,
        &store,
        &dispatching,
        &Arc::new(DashSet::new()),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::New,
        "task must be auto-unblocked even with count=1 from a different reason"
    );
    // Count is reset to 0, then incremented to 1 for the new reason
    assert_eq!(task.auto_unblock_count, 1);
    assert_eq!(
        task.auto_unblock_last_reason, "RateLimit",
        "reason must be updated to the new failure category"
    );
}

/**
 * A task blocked for "exceeded max attempts" has already run the configured number
 * of times. Auto-unblock must leave it alone — flipping it back to new/needs_review
 * on the next tick restarted the very loop that produced the block (#3783)
 **/
#[serial(cooldown_state)]
#[tokio::test]
async fn auto_unblock_leaves_max_attempts_blocked_task_alone() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "3783",
            title: "Approved work sent back to the agent",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Blocked).await.unwrap();

    let run_id = store
        .start_run(&crate::store::StartRun {
            task_id: id,
            attempt: 11,
            run_type: "agent",
            agent: "claude",
            model: "opus",
            command: "claude",
            prompt: "fix the bug",
        })
        .await
        .unwrap();
    store
        .complete_run(&crate::store::CompleteRun {
            run_id,
            exit_code: Some(1),
            stdout: "",
            stderr: "",
            parsed: "",
            outcome: "error",
            error: "exceeded max attempts (11/10). Use `/retry` to reset.",
            tokens: RunTokenUsage::default(),
        })
        .await
        .unwrap();

    auto_unblock_blocked_tasks(
        "owner/repo",
        &task_manager,
        &store,
        &dispatching,
        &Arc::new(DashSet::new()),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::Blocked,
        "max-attempts block must survive the next tick until a person acts"
    );
    assert_eq!(
        task.auto_unblock_count, 0,
        "no auto-unblock attempt should be recorded"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn auto_unblock_stores_reason_on_first_unblock() {
    cooldown::reset_global_state().await;
    // When a task is auto-unblocked for the first time, the failure reason
    // should be stored so subsequent blocks can be compared.
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "1227b",
            title: "Task for first-time reason storage",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Blocked).await.unwrap();

    let run_id = store
        .start_run(&crate::store::StartRun {
            task_id: id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "opus",
            command: "claude",
            prompt: "fix the bug",
        })
        .await
        .unwrap();
    store
        .complete_run(&crate::store::CompleteRun {
            run_id,
            exit_code: Some(1),
            stdout: "",
            stderr: "",
            parsed: "",
            outcome: "timeout",
            error: "task timed out",
            tokens: RunTokenUsage::default(),
        })
        .await
        .unwrap();

    auto_unblock_blocked_tasks(
        "owner/repo",
        &task_manager,
        &store,
        &dispatching,
        &Arc::new(DashSet::new()),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::New);
    assert_eq!(task.auto_unblock_count, 1);
    assert_eq!(
        task.auto_unblock_last_reason, "Timeout",
        "reason must be stored on first auto-unblock"
    );
}

fn make_task(block_reason: Option<String>) -> Task {
    Task {
        id: 1,
        external_id: None,
        repo: "owner/repo".to_string(),
        origin: String::new(),
        title: String::new(),
        body: String::new(),
        status: TaskStatus::Blocked,
        source: String::new(),
        source_id: String::new(),
        author: String::new(),
        url: String::new(),
        labels: vec![],
        agent: None,
        model: None,
        complexity: String::new(),
        estimate: 0,
        route_reason: String::new(),
        agent_profile: String::new(),
        route_attempts: 0,
        attempts: 0,
        branch: String::new(),
        worktree: String::new(),
        worktree_cleaned: false,
        owner_node: None,
        summary: String::new(),
        last_error: String::new(),
        parent_id: None,
        block_reason,
        pr_number: Some(42),
        pr_review_context: String::new(),
        last_review_ts: String::new(),
        review_ts_map: String::new(),
        last_comment_review_ts: String::new(),
        merge_conflict_retries: 0,
        ci_merge_failures: 0,
        pr_create_failures: 0,
        push_failures: 0,
        network_retries: 0,
        timeout_retries: 0,
        review_agent_failures: 0,
        review_resets: 0,
        review_cycles: 0,
        review_invocations: 0,
        needs_review_refires: 0,
        input_tokens: 0,
        output_tokens: 0,
        total_cost_usd: 0.0,
        model_reroute_chain: String::new(),
        limit_reroute_chain: String::new(),
        memory: vec![],
        delegations: vec![],
        auto_unblock_count: 0,
        auto_unblock_last_at: String::new(),
        auto_unblock_last_reason: String::new(),
        ci_recovery_count: 0,
        no_code_reroutes: 0,
        no_code_last_agent: String::new(),
        created_at: String::new(),
        updated_at: String::new(),
    }
}

#[serial(cooldown_state)]
#[test]
fn is_ci_failure_block_matches_ci_failure_limit() {
    let task = make_task(Some(ci_failure_limit_reason(CI_FAILURE_AUTO_MERGE_STAGE)));
    assert!(is_ci_failure_block(&task));
}

#[serial(cooldown_state)]
#[test]
fn is_ci_failure_block_matches_ci_checks_timeout() {
    let task = make_task(Some(
        "CI checks timed out after 3 auto-merge failures".to_string(),
    ));
    assert!(is_ci_failure_block(&task));
}

#[serial(cooldown_state)]
#[test]
fn is_ci_failure_block_false_for_before_review() {
    let task = make_task(Some(ci_failure_limit_reason(
        CI_FAILURE_BEFORE_REVIEW_STAGE,
    )));
    assert!(
        !is_ci_failure_block(&task),
        "pre-review CI blocks must not enter the recovery sweep"
    );
}

#[serial(cooldown_state)]
#[test]
fn is_ci_failure_block_false_for_bootstrap() {
    let task = make_task(Some(
        "CI failure limit (3) reached after the CI bootstrap merge".to_string(),
    ));
    assert!(!is_ci_failure_block(&task));
}

#[serial(cooldown_state)]
#[test]
fn is_ci_failure_block_false_for_regular_block() {
    let task = make_task(Some("waiting on input".to_string()));
    assert!(!is_ci_failure_block(&task));
}

#[serial(cooldown_state)]
#[test]
fn is_ci_failure_block_false_for_none_reason() {
    let task = make_task(None);
    assert!(!is_ci_failure_block(&task));
}

#[serial(cooldown_state)]
#[test]
fn ci_failure_cooldown_zero_count_immediate() {
    // count=0 means never attempted — always allow immediate attempt
    let task = make_task(None);
    assert!(ci_failure_unblock_cooldown_elapsed(&task));
}

#[serial(cooldown_state)]
#[test]
fn ci_failure_cooldown_empty_last_at_immediate() {
    // count=1 but empty last_at means it was never recorded — allow immediate
    let mut task = make_task(None);
    task.auto_unblock_count = 1;
    assert!(ci_failure_unblock_cooldown_elapsed(&task));
}

#[serial(cooldown_state)]
#[test]
fn ci_failure_cooldown_invalid_timestamp_immediate() {
    // invalid timestamp should not block
    let mut task = make_task(None);
    task.auto_unblock_count = 1;
    task.auto_unblock_last_at = "not-a-timestamp".to_string();
    assert!(ci_failure_unblock_cooldown_elapsed(&task));
}

#[serial(cooldown_state)]
#[test]
fn ci_failure_cooldown_high_count_still_eventually_elapses() {
    // count >= 3 must NOT be a permanent block — verify a sufficiently old
    // timestamp eventually elapses the cooldown.
    let long_ago = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
    for count in [3i32, 5, 99] {
        let mut task = make_task(None);
        task.auto_unblock_count = count;
        task.auto_unblock_last_at = long_ago.clone();
        assert!(
            ci_failure_unblock_cooldown_elapsed(&task),
            "count={count} should eventually elapse"
        );
    }
}

#[serial(cooldown_state)]
#[test]
fn ci_failure_cooldown_recent_timestamp_still_blocks() {
    // A recent timestamp should still gate the check at every count.
    let just_now = chrono::Utc::now().to_rfc3339();
    for count in [1i32, 2, 3, 99] {
        let mut task = make_task(None);
        task.auto_unblock_count = count;
        task.auto_unblock_last_at = just_now.clone();
        assert!(
            !ci_failure_unblock_cooldown_elapsed(&task),
            "count={count} should still be cooling down"
        );
    }
}

#[serial(cooldown_state)]
#[tokio::test]
async fn global_ci_failure_sweep_returns_ok_with_no_blocked_tasks() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    auto_unblock_ci_failure_blocked_tasks_global(&task_manager, &store, &Arc::new(DashSet::new()))
        .await
        .unwrap();
}

#[serial(cooldown_state)]
#[tokio::test]
async fn global_ci_failure_sweep_skips_tasks_in_cooldown() {
    // Tasks with a fresh auto_unblock_last_at are skipped before reaching GhHttp,
    // so this test works without a real GitHub token.
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "active/repo".to_string(),
    ));

    // Insert a CI-failure task from an inactive repo with a very recent last_at.
    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "inactive/repo",
            ext_id: "10",
            title: "Old blocked task",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Blocked).await.unwrap();
    let just_now = chrono::Utc::now().to_rfc3339();
    store
        .set_fields(
            id,
            &[
                (
                    "block_reason",
                    serde_json::json!("CI failure limit reached during auto-merge"),
                ),
                ("pr_number", serde_json::json!(99)),
                ("auto_unblock_count", serde_json::json!(1)),
                ("auto_unblock_last_at", serde_json::json!(just_now)),
            ],
        )
        .await
        .unwrap();

    auto_unblock_ci_failure_blocked_tasks_global(&task_manager, &store, &Arc::new(DashSet::new()))
        .await
        .unwrap();

    // Cooldown not elapsed — task must remain untouched.
    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.auto_unblock_count, 1,
        "task in cooldown must not be processed"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn global_ci_failure_sweep_ignores_pre_review_blocks() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "active/repo".to_string(),
    ));

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "inactive/repo",
            ext_id: "21",
            title: "CI failed before review",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Blocked).await.unwrap();
    let reason = ci_failure_limit_reason(CI_FAILURE_BEFORE_REVIEW_STAGE);
    store
        .set_fields(
            id,
            &[
                ("block_reason", serde_json::json!(&reason)),
                ("pr_number", serde_json::json!(4102)),
            ],
        )
        .await
        .unwrap();

    auto_unblock_ci_failure_blocked_tasks_global(&task_manager, &store, &Arc::new(DashSet::new()))
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(task.block_reason.as_deref(), Some(reason.as_str()));
    assert_eq!(
        task.auto_unblock_count, 0,
        "pre-review CI blocks must not enter recovery"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn global_ci_failure_sweep_ignores_non_ci_failure_blocks() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "active/repo".to_string(),
    ));

    // Insert a non-CI-failure blocked task — should be ignored entirely.
    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "inactive/repo",
            ext_id: "20",
            title: "Manual block",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Blocked).await.unwrap();
    store
        .set_fields(
            id,
            &[("block_reason", serde_json::json!("waiting for human input"))],
        )
        .await
        .unwrap();

    auto_unblock_ci_failure_blocked_tasks_global(&task_manager, &store, &Arc::new(DashSet::new()))
        .await
        .unwrap();

    // Non-CI-failure tasks must remain blocked.
    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Blocked);
}

#[serial(cooldown_state)]
#[tokio::test]
async fn global_rebroadcast_sweep_returns_ok_with_no_blocked_tasks() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let router = router_with_healthy_review_agent();
    auto_recover_rebroadcast_blocked_tasks_global(&router, &task_manager, &store, &sweep_repos())
        .await
        .unwrap();
}

#[serial(cooldown_state)]
#[tokio::test]
async fn global_rebroadcast_sweep_ignores_non_rebroadcast_blocks() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "active/repo".to_string(),
    ));
    let router = router_with_healthy_review_agent();

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "inactive/repo",
            ext_id: "30",
            title: "Manual block",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Blocked).await.unwrap();
    store
        .set_fields(
            id,
            &[("block_reason", serde_json::json!("waiting for human input"))],
        )
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    auto_recover_rebroadcast_blocked_tasks_global(&router, &task_manager, &store, &sweep_repos())
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(
        task.block_reason.as_deref(),
        Some("waiting for human input")
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn global_rebroadcast_sweep_skips_freshly_blocked_tasks() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "active/repo".to_string(),
    ));
    let router = router_with_healthy_review_agent();

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "inactive/repo",
            ext_id: "40",
            title: "Just escalated",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Blocked).await.unwrap();
    store
        .set_fields(
            id,
            &[("block_reason", serde_json::json!(REBROADCAST_BLOCK_REASON))],
        )
        .await
        .unwrap();
    // No backdating — escalated "just now" in this same tick, must not be
    // immediately un-blocked by the same pass.

    auto_recover_rebroadcast_blocked_tasks_global(&router, &task_manager, &store, &sweep_repos())
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Blocked);
}

/// Regression test for #3532: a rebroadcast-blocked task whose repo is not
/// the TaskManager's own repo must still be recovered. The sweep queries
/// across all repos and writes through the store directly, bypassing the
/// repo-scoped TaskManager
#[serial(cooldown_state)]
#[tokio::test]
async fn global_rebroadcast_sweep_recovers_task_from_inactive_repo() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "active/repo".to_string(),
    ));
    let router = router_with_healthy_review_agent();

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "inactive/repo",
            ext_id: "458",
            title: "Rebroadcast blocked from inactive repo",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Blocked).await.unwrap();
    store
        .set_fields(
            id,
            &[
                ("block_reason", serde_json::json!(REBROADCAST_BLOCK_REASON)),
                ("needs_review_refires", serde_json::json!(6)),
            ],
        )
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    auto_recover_rebroadcast_blocked_tasks_global(&router, &task_manager, &store, &sweep_repos())
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::NeedsReview,
        "cross-repo rebroadcast-blocked task must be recovered to needs_review"
    );
    assert!(
        task.block_reason.is_none(),
        "block_reason must be cleared on recovery"
    );
    assert_eq!(
        task.needs_review_refires, 0,
        "needs_review_refires must be reset to 0 on recovery"
    );
}

/// Runs `try_unblock_ci_failure_task` against a mock GitHub that reports the
/// given PR state, for a task blocked on a CI failure with PR #7
async fn ci_failure_unblock_with_pr(state: &str, merged: bool) -> (Task, bool) {
    ci_failure_unblock(state, merged, "CI failure limit reached during auto-merge").await
}

async fn ci_failure_unblock_open_with_reason(reason: &str) -> (Task, bool) {
    ci_failure_unblock("open", false, reason).await
}

#[expect(
    clippy::await_holding_lock,
    reason = "the env lock stays held across the awaits so tests do not race on GH_TOKEN"
)]
async fn ci_failure_unblock(state: &str, merged: bool, block_reason: &str) -> (Task, bool) {
    let _env_guard = ENV_LOCK.lock().unwrap();
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("GH_TOKEN", "test_token") };

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "number": 7,
            "title": "t",
            "body": null,
            "state": state,
            "head": { "ref": "task-branch", "sha": "abc" },
            "base": { "ref": "main", "sha": "def" },
            "mergeable": null,
            "mergeable_state": null,
            "merged": merged,
            "html_url": "https://example.test/o/r/pull/7"
        })))
        .mount(&server)
        .await;
    let gh = GhHttp::for_test(server.uri(), None);

    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "o/r".to_string(),
    ));
    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "o/r",
            ext_id: "900",
            title: "CI failure blocked",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Blocked).await.unwrap();
    store
        .set_fields(
            id,
            &[
                ("pr_number", serde_json::json!(7)),
                ("block_reason", serde_json::json!(block_reason)),
            ],
        )
        .await
        .unwrap();
    let task = store.get(id).await.unwrap();

    let unblocked = try_unblock_ci_failure_task(
        &gh,
        "o/r",
        &task_manager,
        &store,
        &task,
        &Arc::new(DashSet::new()),
    )
    .await
    .unwrap();
    (store.get(id).await.unwrap(), unblocked)
}

#[serial(cooldown_state)]
#[tokio::test]
async fn ci_failure_recovery_keeps_a_task_blocked_when_its_pr_closed_unmerged() {
    let (task, unblocked) = ci_failure_unblock_with_pr("closed", false).await;

    assert!(!unblocked);
    assert_eq!(task.status, TaskStatus::Blocked);
    let reason = task.block_reason.unwrap_or_default();
    assert!(
        reason.contains("PR #7 closed unmerged"),
        "block_reason must record the closed PR, got: {reason}"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn ci_failure_recovery_marks_a_task_done_when_its_pr_merged() {
    let (task, unblocked) = ci_failure_unblock_with_pr("closed", true).await;

    assert!(unblocked);
    assert_eq!(task.status, TaskStatus::Done);
    assert!(task.block_reason.is_none());
}

#[serial(cooldown_state)]
#[tokio::test]
async fn ci_failure_recovery_skips_an_open_pr_with_no_approving_review() {
    let reason = ci_failure_limit_reason(CI_FAILURE_BEFORE_REVIEW_STAGE);
    let (task, unblocked) = ci_failure_unblock_open_with_reason(&reason).await;

    assert!(!unblocked);
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(task.block_reason.as_deref(), Some(reason.as_str()));
    assert_eq!(
        task.auto_unblock_count, 0,
        "unreviewed PRs must not arm auto-merge or count a recovery attempt"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn ci_failure_recovery_skips_a_rewritten_reason_without_approval() {
    let reason = "CI failure limit reached during auto-merge (PR #7 still open, state: open)";
    let (task, unblocked) = ci_failure_unblock_open_with_reason(reason).await;

    assert!(!unblocked);
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(task.block_reason.as_deref(), Some(reason));
    assert_eq!(
        task.auto_unblock_count, 0,
        "a rewritten merge-time reason without a recorded approval must not recover"
    );
}

/// With no routable review agent, the sweep must not reset the task, that
/// would immediately re-exhaust the refire counter
#[serial(cooldown_state)]
#[tokio::test]
async fn global_rebroadcast_sweep_skips_reset_without_routable_review_agent() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "active/repo".to_string(),
    ));
    // RouterConfig::default() has no "review" model map entries, so the one
    // available agent has no healthy model for "review" — nothing is routable.
    let router = Router::new_for_test(RouterConfig::default(), vec!["claude".to_string()]);

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "inactive/repo",
            ext_id: "459",
            title: "Rebroadcast blocked, no routable agents",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Blocked).await.unwrap();
    store
        .set_fields(
            id,
            &[("block_reason", serde_json::json!(REBROADCAST_BLOCK_REASON))],
        )
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    auto_recover_rebroadcast_blocked_tasks_global(&router, &task_manager, &store, &sweep_repos())
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(task.block_reason.as_deref(), Some(REBROADCAST_BLOCK_REASON));
}
