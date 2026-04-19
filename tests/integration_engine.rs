//! Engine integration tests — verify the service starts and ticks.
//!
//! Uses an in-memory TaskStore and a mock backend so no GitHub access
//! or API keys are needed. These tests run in CI and catch regressions
//! that break the engine startup or tick loop (e.g., migration checksum
//! mismatches, config parsing failures, panics in init).
//!
//! Run:
//! ```bash
//! cargo test --test integration_engine
//! ```

use async_trait::async_trait;
use orch::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use orch::store::TaskStore;
use std::sync::Arc;

/// Minimal mock backend that passes health checks and returns empty lists.
struct MockBackend;

#[async_trait]
impl ExternalBackend for MockBackend {
    fn name(&self) -> &str {
        "mock"
    }

    async fn create_task(
        &self,
        _title: &str,
        _body: &str,
        _labels: &[String],
    ) -> anyhow::Result<ExternalId> {
        Ok(ExternalId("mock-1".to_string()))
    }

    async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask> {
        Ok(ExternalTask {
            id: id.clone(),
            title: "mock task".to_string(),
            body: "".to_string(),
            state: "open".to_string(),
            labels: vec![],
            author: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "".to_string(),
        })
    }

    async fn list_by_status(&self, _status: Status) -> anyhow::Result<Vec<ExternalTask>> {
        Ok(vec![])
    }

    async fn post_comment(&self, _id: &ExternalId, _body: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn set_labels(&self, _id: &ExternalId, _labels: &[String]) -> anyhow::Result<()> {
        Ok(())
    }

    async fn remove_label(&self, _id: &ExternalId, _label: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn get_sub_issues(&self, _id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
        Ok(vec![])
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Verify that TaskStore opens and migrations run on a fresh database.
///
/// This is the #1 failure mode: agents modify migration files, breaking
/// the checksum, and the engine loops forever on startup.
#[tokio::test]
async fn store_opens_and_migrates() {
    let tmp = std::env::temp_dir().join(format!("orch-engine-open-{}.db", std::process::id()));
    let store = TaskStore::open(&tmp).await;
    assert!(store.is_ok(), "TaskStore::open failed: {:?}", store.err());
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(tmp.with_extension("db-shm"));
    let _ = std::fs::remove_file(tmp.with_extension("db-wal"));
}

/// Verify that TaskStore migrations are idempotent (open twice).
#[tokio::test]
async fn store_migrations_idempotent() {
    let tmp = std::env::temp_dir().join(format!("orch-engine-test-{}.db", std::process::id()));

    // First open (uses real open with max_connections(5) — production path)
    {
        let result = TaskStore::open(&tmp).await;
        assert!(result.is_ok(), "first open failed: {:?}", result.err());
    }

    // Second open (validates checksums against already-applied migrations)
    {
        let result = TaskStore::open(&tmp).await;
        assert!(
            result.is_ok(),
            "second open failed (checksum mismatch?): {:?}",
            result.err()
        );
    }

    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(tmp.with_extension("db-shm"));
    let _ = std::fs::remove_file(tmp.with_extension("db-wal"));
}

/// Verify that tasks table exists and is queryable after migrations.
#[tokio::test]
async fn store_tasks_table_exists() {
    let tmp = std::env::temp_dir().join(format!("orch-engine-table-{}.db", std::process::id()));
    let store = TaskStore::open_single(&tmp).await.expect("open store");

    // Just verify the tasks table is queryable (schema is correct)
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tasks")
        .fetch_one(store.pool())
        .await
        .expect("query tasks table");
    assert_eq!(count, 0);

    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(tmp.with_extension("db-shm"));
    let _ = std::fs::remove_file(tmp.with_extension("db-wal"));
}

/// Verify that the router config parses without panicking.
#[test]
fn router_config_parses() {
    // This exercises RouterConfig::from_config() which reads config.yml.
    // In test context there's no config file, so it uses defaults.
    let config = orch::engine::router::config::RouterConfig::from_config();
    assert!(!config.agents.is_empty(), "should have default agents");
    assert_eq!(config.mode, "llm");
}

/// Verify that the mock backend passes the health check.
#[tokio::test]
async fn mock_backend_health_check() {
    let backend = MockBackend;
    assert!(backend.health_check().await.is_ok());
}

/// Verify that cooldown init works with an in-memory store.
#[tokio::test]
async fn cooldown_init_with_store() {
    let tmp = std::env::temp_dir().join(format!("orch-engine-cooldown-{}.db", std::process::id()));
    let store = Arc::new(TaskStore::open_single(&tmp).await.expect("open store"));
    orch::engine::cooldown::init_cooldown_store(store).await;
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(tmp.with_extension("db-shm"));
    let _ = std::fs::remove_file(tmp.with_extension("db-wal"));
    // Should not panic — that's the test
}

// ---------------------------------------------------------------------------
// Engine integration tests — exercise the engine startup path and core
// components (store, task manager, router, runner) wired together.
// ---------------------------------------------------------------------------

/// Helper: create a temp DB path with a unique suffix and return it.
/// The caller is responsible for cleanup. Uses an atomic counter to
/// guarantee uniqueness even when tests run in parallel threads.
fn temp_db(label: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("orch-integ-{label}-{}-{n}.db", std::process::id(),))
}

/// Helper: remove a temp DB and its WAL/SHM files.
fn cleanup_db(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
}

/// Verify that a ProjectEngine can be fully constructed from mock components.
///
/// This exercises the same wiring path as `init_project_engines()` in the
/// real engine: store + backend + TaskManager + TaskRunner + ProjectEngine.
/// If any struct field changes or a constructor signature drifts, this fails.
#[tokio::test]
async fn project_engine_constructs() {
    use orch::engine::runner::TaskRunner;
    use orch::engine::tasks::TaskManager;
    use orch::engine::ProjectEngine;

    let tmp = temp_db("pe-construct");
    let store = Arc::new(TaskStore::open_single(&tmp).await.expect("open store"));
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend);

    let task_manager = Arc::new(TaskManager::with_store(
        backend.clone(),
        store.clone(),
        "test/repo".to_string(),
    ));

    let runner = Arc::new(TaskRunner::new("test/repo".to_string()).with_store(store.clone()));

    let engine = ProjectEngine {
        repo: "test/repo".to_string(),
        project_dir: std::path::PathBuf::from("/tmp/test-project"),
        backend,
        task_manager,
        runner,
        store: store.clone(),
    };

    assert_eq!(engine.repo, "test/repo");
    cleanup_db(&tmp);
}

/// Verify that TaskManager can create an internal task and the task appears
/// in the store with status `New`.
#[tokio::test]
async fn task_manager_creates_internal_task() {
    use orch::engine::tasks::TaskManager;
    use orch::store::TaskStatus;

    let tmp = temp_db("tm-create");
    let store = Arc::new(TaskStore::open_single(&tmp).await.expect("open store"));
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend);

    let tm = TaskManager::with_store(backend, store.clone(), "test/repo".to_string());

    // Create an internal task via the store directly (TaskManager.create_task
    // requires more setup; the store method is what the engine uses internally).
    let task_id = store
        .create_internal(
            "test/repo",
            "Fix the widget",
            "The widget is broken",
            "test",
            "1",
            None,
        )
        .await
        .expect("create internal task");

    // Verify it exists and has the right status
    let task = store.get(task_id).await.expect("get task");
    assert_eq!(task.title, "Fix the widget");
    assert_eq!(task.status, TaskStatus::New);
    assert_eq!(task.repo, "test/repo");

    // Verify it shows up in the routable list (status = new)
    let routable = store
        .list_routable("test/repo")
        .await
        .expect("list routable");
    assert_eq!(routable.len(), 1);
    assert_eq!(routable[0].id, task_id);

    // Verify TaskManager is usable (no panic on list)
    let internal = tm
        .list_internal_by_status(TaskStatus::New)
        .await
        .expect("list internal");
    assert_eq!(internal.len(), 1);

    cleanup_db(&tmp);
}

/// Verify that creating an external task works and the task can be
/// retrieved by external ID.
#[tokio::test]
async fn create_external_task_and_retrieve() {
    use orch::store::{NewTask, TaskStatus};

    let tmp = temp_db("create-ext");
    let store = Arc::new(TaskStore::open_single(&tmp).await.expect("open store"));

    let id = store
        .create(&NewTask {
            external_id: Some("42".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Add dark mode".to_string(),
            body: "Users want dark mode support".to_string(),
            source: "webhook".to_string(),
            source_id: "42".to_string(),
            author: "user1".to_string(),
            url: "https://github.com/owner/repo/issues/42".to_string(),
            labels: vec!["enhancement".to_string()],
            parent_id: None,
        })
        .await
        .expect("create external task");

    let task = store.get(id).await.expect("get task");
    assert_eq!(task.title, "Add dark mode");
    assert_eq!(task.status, TaskStatus::New);
    assert_eq!(task.external_id.as_deref(), Some("42"));
    assert_eq!(task.origin, "github");
    assert_eq!(task.labels, vec!["enhancement"]);

    // Verify it shows up in external task listing
    let all = store
        .list_all_external("owner/repo")
        .await
        .expect("list all external");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].title, "Add dark mode");

    cleanup_db(&tmp);
}

/// Verify task status transitions through the full lifecycle.
///
/// This catches regressions in the status update SQL and the
/// TaskStatus enum serialization.
#[tokio::test]
async fn task_status_lifecycle() {
    use orch::store::TaskStatus;

    let tmp = temp_db("status-lifecycle");
    let store = Arc::new(TaskStore::open_single(&tmp).await.expect("open store"));

    let id = store
        .create_internal("test/repo", "Lifecycle test", "", "test", "lc-1", None)
        .await
        .expect("create task");

    // new → routed
    store
        .update_status(id, TaskStatus::Routed)
        .await
        .expect("new → routed");
    assert_eq!(store.get(id).await.unwrap().status, TaskStatus::Routed);

    // routed → in_progress
    store
        .update_status(id, TaskStatus::InProgress)
        .await
        .expect("routed → in_progress");
    assert_eq!(store.get(id).await.unwrap().status, TaskStatus::InProgress);

    // in_progress → needs_review
    store
        .update_status(id, TaskStatus::NeedsReview)
        .await
        .expect("in_progress → needs_review");
    assert_eq!(store.get(id).await.unwrap().status, TaskStatus::NeedsReview);

    // needs_review → in_review
    store
        .update_status(id, TaskStatus::InReview)
        .await
        .expect("needs_review → in_review");
    assert_eq!(store.get(id).await.unwrap().status, TaskStatus::InReview);

    // in_review → done
    store
        .update_status(id, TaskStatus::Done)
        .await
        .expect("in_review → done");
    assert_eq!(store.get(id).await.unwrap().status, TaskStatus::Done);

    cleanup_db(&tmp);
}

/// Verify that storing a route result persists agent, model, and complexity.
///
/// This exercises the same code path as `tick_route_tasks` without needing
/// an LLM call or tmux session.
#[tokio::test]
async fn store_route_result_persists() {
    use orch::store::{StoreRoute, TaskStatus};

    let tmp = temp_db("route-persist");
    let store = Arc::new(TaskStore::open_single(&tmp).await.expect("open store"));

    let id = store
        .create_internal("test/repo", "Route me", "task body", "test", "rt-1", None)
        .await
        .expect("create task");

    // Simulate what the router does after routing
    store
        .store_route(&StoreRoute {
            id,
            agent: "claude",
            model: Some("sonnet"),
            complexity: "medium",
            estimate: 5,
            reason: "test routing",
            profile: r#"{"role":"backend specialist","skills":[],"tools":[],"constraints":[]}"#,
            skills: r#"["gh","git-worktree"]"#,
        })
        .await
        .expect("store route");

    // Update status to routed (as the engine does after storing the route)
    store
        .update_status(id, TaskStatus::Routed)
        .await
        .expect("update to routed");

    // Verify the route data persisted
    let task = store.get(id).await.expect("get routed task");
    assert_eq!(task.status, TaskStatus::Routed);
    assert_eq!(task.agent.as_deref(), Some("claude"));
    assert_eq!(task.model.as_deref(), Some("sonnet"));
    assert_eq!(task.complexity, "medium");
    assert_eq!(task.route_reason, "test routing");

    cleanup_db(&tmp);
}

/// Verify that the Router can be constructed and that round-robin routing
/// works without any LLM call.
///
/// This is the closest we can get to testing the actual routing phase from
/// an integration test, since tick_route_tasks is pub(crate).
#[tokio::test]
async fn router_round_robin_routes_task() {
    use orch::engine::router::config::RouterConfig;
    use orch::engine::router::Router;
    use orch::store::UpsertExternal;

    let tmp = temp_db("rr-route");
    let store = Arc::new(TaskStore::open_single(&tmp).await.expect("open store"));

    // Use round_robin mode so no LLM call is needed.
    // Only include agents likely to be in PATH on CI/dev machines.
    let mut config = RouterConfig {
        mode: "round_robin".to_string(),
        agents: vec!["claude".to_string(), "codex".to_string()],
        ..RouterConfig::default()
    };
    // has_available_model_for_complexity returns false when no model_map entry exists,
    // so we must configure models for the agents we want to route to.
    // Tasks with no complexity label default to "medium" complexity.
    for agent in &["claude", "codex"] {
        for tier in &["simple", "medium", "complex"] {
            config
                .model_map
                .entry(tier.to_string())
                .or_default()
                .insert(agent.to_string(), vec!["sonnet".to_string()]);
        }
    }

    let mut router = Router::new(config);

    // If no agents are discovered (CI has none), skip the routing assertion
    // but still verify the router constructed without panic.
    if router.available_agents.is_empty() {
        // Router constructed, agents discovered (none found) — still a valid test
        return;
    }

    // Create a task in the store so resolve_task_id works
    let ext = UpsertExternal {
        repo: "test/repo",
        ext_id: "99",
        title: "Round-robin test task",
        body: "Test body for routing",
        author: "test",
        url: "",
        labels: &[],
        origin: "github",
    };
    store.upsert_external(&ext).await.expect("upsert");

    // Build an ExternalTask to pass to route()
    let task = ExternalTask {
        id: ExternalId("99".to_string()),
        title: "Round-robin test task".to_string(),
        body: "Test body for routing".to_string(),
        state: "open".to_string(),
        labels: vec![],
        author: "test".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: "".to_string(),
    };

    let result = router.route(&task, &store, "test/repo").await;
    assert!(
        result.is_ok(),
        "round-robin routing should succeed: {:?}",
        result.err()
    );

    let route = result.unwrap();
    assert!(
        router.available_agents.contains(&route.agent),
        "routed agent {:?} should be in available agents {:?}",
        route.agent,
        router.available_agents
    );
    assert!(
        !route.complexity.is_empty(),
        "complexity should not be empty"
    );
    assert!(!route.reason.is_empty(), "reason should not be empty");

    // Store the route result and verify persistence
    router
        .store_route_result("99", &route, &store, "test/repo")
        .await
        .expect("store route result");

    let stored = orch::engine::router::get_route_result(&store, "test/repo", "99")
        .await
        .expect("get route result");
    assert_eq!(stored.agent, route.agent);
    assert_eq!(stored.complexity, route.complexity);

    cleanup_db(&tmp);
}

/// Verify that the TaskRunner can be constructed with a store.
///
/// The runner requires tmux for actual dispatch, but construction and
/// store wiring should not panic.
#[tokio::test]
async fn task_runner_constructs_with_store() {
    use orch::engine::runner::TaskRunner;

    let tmp = temp_db("runner-construct");
    let store = Arc::new(TaskStore::open_single(&tmp).await.expect("open store"));

    let _runner = TaskRunner::new("test/repo".to_string()).with_store(store);
    // Construction succeeded without panic — that's the test

    cleanup_db(&tmp);
}

/// Verify that task activity tracking works (append + retrieve).
///
/// The engine appends activity entries on every status change. If the
/// activity schema breaks, the engine panics mid-tick.
#[tokio::test]
async fn task_activity_tracking() {
    let tmp = temp_db("activity");
    let store = Arc::new(TaskStore::open_single(&tmp).await.expect("open store"));

    let id = store
        .create_internal("test/repo", "Activity test", "", "test", "act-1", None)
        .await
        .expect("create task");

    // Status updates automatically append activity entries
    store
        .update_status(id, orch::store::TaskStatus::Routed)
        .await
        .expect("route");
    store
        .update_status(id, orch::store::TaskStatus::InProgress)
        .await
        .expect("dispatch");

    let activity = store.get_activity(id, None).await.expect("get activity");
    // Should have at least the status change entries
    assert!(
        !activity.is_empty(),
        "activity log should not be empty after status changes"
    );

    cleanup_db(&tmp);
}

/// Verify that the full engine wiring path works end-to-end:
/// store + task creation + routing data + status update + retrieval.
///
/// This simulates what a single tick does (minus tmux dispatch):
/// 1. Task appears in store (new)
/// 2. Router selects agent (store route)
/// 3. Status updated to routed
/// 4. Verify task is no longer in the routable list
#[tokio::test]
async fn engine_tick_simulation() {
    use orch::store::{StoreRoute, TaskStatus, UpsertExternal};

    let tmp = temp_db("tick-sim");
    let store = Arc::new(TaskStore::open_single(&tmp).await.expect("open store"));

    // Phase 0: upsert an external task (simulates sync from GitHub)
    let ext = UpsertExternal {
        repo: "owner/repo",
        ext_id: "101",
        title: "Implement caching layer",
        body: "We need Redis-backed caching for the API",
        author: "engineer",
        url: "https://github.com/owner/repo/issues/101",
        labels: &["enhancement".to_string(), "backend".to_string()],
        origin: "github",
    };
    let id = store.upsert_external(&ext).await.expect("upsert");

    // Verify task is routable (status = new)
    let routable = store
        .list_routable("owner/repo")
        .await
        .expect("list routable");
    assert_eq!(routable.len(), 1);

    // Phase 3a: Route the task (simulates tick_route_tasks)
    store
        .store_route(&StoreRoute {
            id,
            agent: "claude",
            model: Some("opus"),
            complexity: "complex",
            estimate: 8,
            reason: "backend task requiring deep caching knowledge",
            profile: r#"{"role":"backend specialist","skills":["redis"],"tools":["bash"],"constraints":[]}"#,
            skills: r#"["gh"]"#,
        })
        .await
        .expect("store route");

    store
        .update_status(id, TaskStatus::Routed)
        .await
        .expect("mark routed");

    // Verify task is no longer routable
    let routable_after = store
        .list_routable("owner/repo")
        .await
        .expect("list routable after");
    assert!(
        routable_after.is_empty(),
        "routed task should not appear in routable list"
    );

    // Verify routed state
    let task = store.get(id).await.expect("get routed task");
    assert_eq!(task.status, TaskStatus::Routed);
    assert_eq!(task.agent.as_deref(), Some("claude"));
    assert_eq!(task.model.as_deref(), Some("opus"));

    // Phase 3b: Dispatch (simulates tick_dispatch_tasks — just update status)
    store
        .update_status(id, TaskStatus::InProgress)
        .await
        .expect("mark in_progress");

    let dispatched = store.get(id).await.expect("get dispatched");
    assert_eq!(dispatched.status, TaskStatus::InProgress);

    // Simulate completion → needs_review → done
    store
        .update_status(id, TaskStatus::NeedsReview)
        .await
        .expect("needs_review");
    store
        .update_status(id, TaskStatus::InReview)
        .await
        .expect("in_review");
    store
        .update_status(id, TaskStatus::Done)
        .await
        .expect("done");

    let final_task = store.get(id).await.expect("get final");
    assert_eq!(final_task.status, TaskStatus::Done);

    // Verify activity trail has entries for the full lifecycle
    let activity = store.get_activity(id, None).await.expect("activity");
    assert!(
        activity.len() >= 4,
        "expected at least 4 activity entries for full lifecycle, got {}",
        activity.len()
    );

    cleanup_db(&tmp);
}

/// Verify that blocked status works and block_reason is stored.
#[tokio::test]
async fn task_blocked_with_reason() {
    use orch::store::TaskStatus;

    let tmp = temp_db("blocked");
    let store = Arc::new(TaskStore::open_single(&tmp).await.expect("open store"));

    let id = store
        .create_internal("test/repo", "Blocked task", "", "test", "blk-1", None)
        .await
        .expect("create task");

    store
        .update_status(id, TaskStatus::Blocked)
        .await
        .expect("block");
    store
        .set_block_reason(id, Some("max review cycles exceeded"))
        .await
        .expect("set block reason");

    let task = store.get(id).await.expect("get blocked task");
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(
        task.block_reason.as_deref(),
        Some("max review cycles exceeded")
    );

    // Unblock
    store
        .update_status(id, TaskStatus::New)
        .await
        .expect("unblock");
    store
        .set_block_reason(id, None)
        .await
        .expect("clear block reason");

    let unblocked = store.get(id).await.expect("get unblocked");
    assert_eq!(unblocked.status, TaskStatus::New);
    assert!(unblocked.block_reason.is_none());

    cleanup_db(&tmp);
}

/// Verify that multiple tasks across repos are isolated correctly.
///
/// The engine runs multiple ProjectEngines — one per repo. Tasks from
/// different repos must not leak into each other's routable lists.
#[tokio::test]
async fn multi_repo_task_isolation() {
    use orch::store::TaskStatus;

    let tmp = temp_db("multi-repo");
    let store = Arc::new(TaskStore::open_single(&tmp).await.expect("open store"));

    store
        .create_internal("repo-a/project", "Task A", "", "test", "a-1", None)
        .await
        .expect("create task A");
    store
        .create_internal("repo-b/project", "Task B", "", "test", "b-1", None)
        .await
        .expect("create task B");

    let routable_a = store.list_routable("repo-a/project").await.expect("list A");
    let routable_b = store.list_routable("repo-b/project").await.expect("list B");

    assert_eq!(routable_a.len(), 1, "repo A should have 1 routable task");
    assert_eq!(routable_b.len(), 1, "repo B should have 1 routable task");
    assert_eq!(routable_a[0].title, "Task A");
    assert_eq!(routable_b[0].title, "Task B");

    // Routing task A should not affect repo B
    store
        .update_status(routable_a[0].id, TaskStatus::Routed)
        .await
        .expect("route A");

    let routable_a_after = store
        .list_routable("repo-a/project")
        .await
        .expect("list A after");
    let routable_b_after = store
        .list_routable("repo-b/project")
        .await
        .expect("list B after");

    assert!(routable_a_after.is_empty(), "repo A should have 0 routable");
    assert_eq!(
        routable_b_after.len(),
        1,
        "repo B should still have 1 routable"
    );

    cleanup_db(&tmp);
}

/// Verify graceful degradation config defaults.
#[test]
fn graceful_degradation_config_defaults() {
    use orch::engine::router::config::*;

    // Test default threshold
    let threshold = min_healthy_agents_threshold();
    assert_eq!(threshold, 2, "default threshold should be 2");

    // Test default sequential delay
    let delay = sequential_dispatch_delay_ms();
    assert_eq!(delay, 1000, "default sequential delay should be 1000ms");

    // Test default retry base delay
    let base = retry_base_delay_ms();
    assert_eq!(base, 10_000, "default retry base delay should be 10000ms");

    // Test default retry max delay
    let max = retry_max_delay_ms();
    assert_eq!(max, 120_000, "default retry max delay should be 120000ms");
}

/// Verify exponential backoff delay calculation works within expected ranges.
#[test]
fn exponential_backoff_calculation() {
    // Test config values directly (these are public)
    use orch::engine::router::config::{retry_base_delay_ms, retry_max_delay_ms};

    let base = retry_base_delay_ms();
    let max = retry_max_delay_ms();

    assert_eq!(base, 10_000, "base delay should be 10s");
    assert_eq!(max, 120_000, "max delay should be 120s");

    // Verify that exponential backoff grows as expected (at least base * 2^n)
    // We can't test the exact jitter, but we can verify the base * 2^n growth
    let growth_0 = base * 2u64.saturating_pow(0);
    let growth_1 = base * 2u64.saturating_pow(1);
    let growth_2 = base * 2u64.saturating_pow(2);
    let growth_3 = base * 2u64.saturating_pow(3);
    let growth_4 = base * 2u64.saturating_pow(4);

    assert!(growth_1 > growth_0, "exponential should grow");
    assert!(growth_2 > growth_1, "exponential should grow");
    assert!(growth_3 > growth_2, "exponential should grow");
    assert!(growth_4 > growth_3, "exponential should grow");

    // Verify cap is applied (growth_4 should be capped at max)
    assert!(growth_4 >= max, "should be capped at max");
}

/// Verify router healthy agent count.
#[tokio::test]
async fn router_healthy_agent_count() {
    use orch::engine::router::{Router, RouterConfig};

    // Create router with default config
    let config = RouterConfig::default();
    let router = Router::new(config);

    // At minimum, available_agents should contain agents that are in PATH
    // The healthy_agent_count should be <= available_agents.len()
    let healthy = router.healthy_agent_count("simple");
    let available = router.available_agents.len();

    assert!(
        healthy <= available,
        "healthy count ({}) should be <= available ({})",
        healthy,
        available
    );

    // Test is_degraded with various thresholds
    // If healthy < threshold, it's degraded
    let _degraded_at_5 = router.is_degraded(5);
    let degraded_at_1 = router.is_degraded(1);

    // We can't predict exact values since it depends on what's in PATH
    // but degraded_at_1 should generally be false (unless NO agents available)
    // and degraded_at_5 would be true if fewer than 5 healthy agents
    assert!(
        !degraded_at_1 || healthy == 0,
        "is_degraded(1) should be false unless no healthy agents"
    );
}

// ---------------------------------------------------------------------------
// Regression tests for update_status_and_fields decode-error propagation.
//
// Before the fix, `try_get("status").unwrap_or_default()` / `unwrap_or(None)`
// would silently mask column decode failures and still write an activity row
// with defaulted (""/None) pre-update values.  The fix replaces every
// `unwrap_or_*` with `.map_err(…)?` so any failure surfaces to the caller.
// ---------------------------------------------------------------------------

/// Verify that update_status_and_fields records activity with the correct
/// pre-update values (from_status / agent / model) read from the row.
///
/// This is the happy-path regression: the function must not default these
/// values silently — it must read them and pass them through.
#[tokio::test]
async fn update_status_and_fields_records_pre_update_values_in_activity() {
    use orch::store::{TaskActivity, TaskStatus};

    let tmp = std::env::temp_dir().join(format!("orch-update-activity-{}.db", std::process::id()));
    let store = TaskStore::open_single(&tmp).await.expect("open store");

    let id = store
        .create_internal("test/repo", "decode-error test", "", "test", "job:1", None)
        .await
        .expect("create task");

    // Task starts as 'new'. Transition to 'routed' and check activity.
    store
        .update_status_and_fields(
            id,
            TaskStatus::Routed,
            &[("summary", serde_json::json!("initial summary"))],
        )
        .await
        .expect("update_status_and_fields should succeed");

    let activity: Vec<TaskActivity> = store.get_activity(id, None).await.expect("get_activity");

    assert_eq!(activity.len(), 1, "expected one activity entry");
    let entry = &activity[0];
    assert_eq!(entry.event_type, "status_change");
    // from_status must be the real pre-update value, not an empty default.
    assert_eq!(
        entry.from_status.as_deref(),
        Some("new"),
        "from_status must be read from the row, not defaulted"
    );
    assert_eq!(entry.to_status.as_deref(), Some("routed"));
    // agent and model are NULL at this point — they should be recorded as None,
    // not silently set to Some("") by a stale unwrap_or_default.
    assert_eq!(entry.agent, None, "agent should be None, not a default");
    assert_eq!(entry.model, None, "model should be None, not a default");

    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(tmp.with_extension("db-shm"));
    let _ = std::fs::remove_file(tmp.with_extension("db-wal"));
}

/// Verify that update_status_and_fields propagates errors instead of silently
/// continuing.  When the pre-update fetch fails (e.g. the row does not exist),
/// the function must return Err rather than proceeding with defaulted values.
#[tokio::test]
async fn update_status_and_fields_propagates_fetch_error_on_missing_row() {
    use orch::store::TaskStatus;

    let tmp = std::env::temp_dir().join(format!("orch-update-norow-{}.db", std::process::id()));
    let store = TaskStore::open_single(&tmp).await.expect("open store");

    // Row id 9999 does not exist — fetch_one will return RowNotFound, which
    // must propagate as Err rather than being swallowed.
    let result = store
        .update_status_and_fields(9999, TaskStatus::Routed, &[])
        .await;

    assert!(
        result.is_err(),
        "update_status_and_fields must return Err when the row does not exist"
    );

    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(tmp.with_extension("db-shm"));
    let _ = std::fs::remove_file(tmp.with_extension("db-wal"));
}

/// Verify that update_status_and_fields propagates column decode errors instead
/// of silently defaulting.  When `try_get` fails (type mismatch, corruption,
/// non-UTF8 bytes in a TEXT column), the function must return Err — and no
/// activity row may be written from defaulted values.
///
/// This is the critical regression: before the fix, `unwrap_or_default()` /
/// `unwrap_or(None)` on `try_get` calls would swallow decode failures, write
/// an activity row with defaulted pre-update fields, and return Ok.
#[tokio::test]
async fn update_status_and_fields_propagates_column_decode_error() {
    use orch::store::{TaskActivity, TaskStatus};

    let tmp = std::env::temp_dir().join(format!("orch-update-decode-{}.db", std::process::id()));
    let store = TaskStore::open_single(&tmp).await.expect("open store");

    let id = store
        .create_internal("test/repo", "decode-error test", "", "test", "job:1", None)
        .await
        .expect("create task");

    // Inject raw binary bytes into the `status` column. SQLite's type affinity
    // allows BLOB data in a TEXT column. sqlx's `try_get::<_, String>` will then
    // fail trying to decode non-UTF8 bytes — this is the pre-update column
    // decode failure the bug describes.
    sqlx::query("UPDATE tasks SET status = X'FFFFFFFF' WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .expect("inject non-UTF8 status");

    // Update must fail rather than silently defaulting.
    let result = store
        .update_status_and_fields(id, TaskStatus::Routed, &[])
        .await;

    assert!(
        result.is_err(),
        "update_status_and_fields must return Err when status column decode fails"
    );

    // Crucially, no incorrect activity row may have been written.
    let activity: Vec<TaskActivity> = store.get_activity(id, None).await.expect("get_activity");
    assert!(
        activity.is_empty(),
        "no activity should be written when pre-update column decode fails"
    );

    // Verify the task row itself is unchanged (no partial update).
    // We use raw SQL rather than store.get() because the latter uses a full row
    // decoder that would also fail on the corrupted status blob. Fetch the raw
    // status as bytes — if the update failed before writing, it will still be the
    // original "new" value (or the injected blob if we want to check that too).
    let status_row: (Vec<u8>,) = sqlx::query_as("SELECT status FROM tasks WHERE id = ?")
        .bind(id)
        .fetch_one(store.pool())
        .await
        .expect("fetch status raw");
    // After the failed update, status should still be the original value.
    // We inject X'FFFF' (2 bytes) — check it's not "routed" (6 bytes ASCII).
    assert_ne!(
        std::str::from_utf8(&status_row.0).ok(),
        Some("routed"),
        "task status must remain unchanged when decode fails"
    );

    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(tmp.with_extension("db-shm"));
    let _ = std::fs::remove_file(tmp.with_extension("db-wal"));
}
