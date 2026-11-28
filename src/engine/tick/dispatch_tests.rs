use super::dispatch::block_task_without_project;
use super::dispatch::group_orphaned_routed_tasks;
use super::tests::MockBackend;
use super::*;
use crate::channels::transport::Transport;
use crate::cooldown;
use crate::engine::router::Router;
use crate::engine::router::RouterConfig;
use crate::engine::runner::NoProjectDir;
use crate::engine::tasks::TaskManager;
use crate::store::DepSource;
use crate::store::NewDependency;
use crate::store::StoreRoute;
use crate::store::TaskStatus as DbStatus;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use crate::store::UpsertExternal;
use crate::tmux::TmuxManager;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time;

/// Regression test for #1361: tick_dispatch_tasks previously accepted
/// `&Arc<RwLock<Router>>` and called `router_arc.read().await` internally.
/// The main loop already held a write lock on the same RwLock, so the read
/// could never be acquired — guaranteed deadlock on every tick with routed
/// tasks.
///
/// The fix changed the signature to accept `&Router` directly (the caller
/// already has the dereferenced guard), eliminating the lock re-acquisition.
///
/// This test verifies the function completes within 2 seconds when the
/// caller holds the write lock — the old code would deadlock here.
#[tokio::test]
async fn dispatch_does_not_deadlock_under_write_lock() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let semaphore = Arc::new(Semaphore::new(4));
    let (weight_tx, _weight_rx) = mpsc::channel(16);
    let dispatching = Arc::new(DashMap::new());
    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(transport));

    let router_config = RouterConfig::default();
    let router = Router::new_for_test(router_config, vec!["claude".to_string()]);
    let router_arc = Arc::new(RwLock::new(router));

    // Simulate what the main loop does: hold a write lock, snapshot
    // dispatch mode, then call tick_dispatch_tasks without passing any
    // lock guard into async dispatch work.
    let write_guard = router_arc.write().await;
    let dispatch_mode = dispatch_mode_from_router(&write_guard);

    let result = time::timeout(
        Duration::from_secs(2),
        tick_dispatch_tasks(
            &backend,
            &tmux,
            "owner/repo",
            &Arc::new(TaskRunner::new("owner/repo".to_string())),
            &capture,
            &semaphore,
            &task_manager,
            &weight_tx,
            dispatch_mode,
            &dispatching,
            &store,
            &HashMap::new(),
        ),
    )
    .await;

    assert!(
        result.is_ok(),
        "tick_dispatch_tasks deadlocked! It tried to acquire a read lock \
             on router_arc while a write lock was already held (issue #1361)"
    );
}

/// Regression test for #3599: a task routed to an agent/model that entered
/// cooldown *after* routing but *before* dispatch must not be dispatched —
/// the stale route was decided before the failure that triggered the
/// cooldown was known. Dispatch must re-check the assigned agent/model
/// against the same cooldown store used at routing time and, if stale,
/// leave the task in `Routed` for the next tick to re-evaluate (no PR is
/// spawned, no per-task defer timer is written).
#[tokio::test]
async fn dispatch_skips_task_whose_routed_model_entered_cooldown() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let runner = Arc::new(TaskRunner::new("owner/repo".to_string()));
    let semaphore = Arc::new(Semaphore::new(4));
    let (weight_tx, _weight_rx) = mpsc::channel(16);
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(transport));

    let id = store
        .upsert_external(&UpsertExternal {
            repo: "owner/repo",
            ext_id: "999",
            title: "Stale routed task",
            body: "",
            author: "user",
            url: "https://github.com/owner/repo/issues/999",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .store_route(&StoreRoute {
            id,
            agent: "minimax",
            model: Some("opus"),
            complexity: "medium",
            estimate: 0,
            reason: "test",
            profile: "{}",
        })
        .await
        .unwrap();
    store.update_status(id, DbStatus::Routed).await.unwrap();

    // Simulate a billing/rate-limit cooldown recorded for minimax:opus after
    // this task was routed but before it dispatched.
    cooldown::set_model_cooldown("minimax", "opus", 300, "rate_limit_usage_window").await;

    let dispatch_mode = DispatchMode {
        is_degraded: false,
        healthy_agents: 1,
        threshold: 1,
    };

    tick_dispatch_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &runner,
        &capture,
        &semaphore,
        &task_manager,
        &weight_tx,
        dispatch_mode,
        &dispatching,
        &store,
        &HashMap::new(),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        DbStatus::Routed,
        "task whose routed model is now cooled down must stay Routed, not be dispatched"
    );
    assert!(
        dispatching.is_empty(),
        "dispatch guard must be released when skipping a stale-cooldown task"
    );
}

#[tokio::test]
async fn dispatch_skips_a_task_with_an_open_dependency() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let runner = Arc::new(TaskRunner::new("owner/repo".to_string()));
    let semaphore = Arc::new(Semaphore::new(4));
    let (weight_tx, _weight_rx) = mpsc::channel(16);
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(transport));

    let id = store
        .upsert_external(&UpsertExternal {
            repo: "owner/repo",
            ext_id: "42",
            title: "Waiting routed task",
            body: "",
            author: "user",
            url: "https://github.com/owner/repo/issues/42",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .replace_dependencies(
            id,
            &[NewDependency {
                repo: "owner/repo".into(),
                number: 7,
                source: DepSource::Github,
            }],
        )
        .await
        .unwrap();
    store
        .store_route(&StoreRoute {
            id,
            agent: "claude",
            model: Some("haiku"),
            complexity: "simple",
            estimate: 0,
            reason: "test",
            profile: "{}",
        })
        .await
        .unwrap();
    store.update_status(id, DbStatus::Routed).await.unwrap();

    let dispatch_mode = DispatchMode {
        is_degraded: false,
        healthy_agents: 1,
        threshold: 1,
    };
    tick_dispatch_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &runner,
        &capture,
        &semaphore,
        &task_manager,
        &weight_tx,
        dispatch_mode,
        &dispatching,
        &store,
        &HashMap::new(),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, DbStatus::Waiting);
    assert!(dispatching.is_empty());
}

/// A task of a repo with no project directory ends `blocked` with a reason that names
/// the cause, never `needs_review` (#4024)
#[tokio::test]
async fn block_task_without_project_blocks_with_the_cause() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "removed/repo".to_string(),
    ));
    let id = store
        .create_internal("removed/repo", "Orphan", "", "cron", "1", None)
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Routed).await.unwrap();

    block_task_without_project(
        &task_manager,
        &store,
        "removed/repo",
        &format!("internal:{id}"),
    )
    .await;

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(
        task.block_reason.as_deref(),
        Some(NoProjectDir::BLOCK_REASON)
    );
}

/// Regression test for #3413: tasks routed for an inactive/removed repo were
/// stuck in `routed` indefinitely because `tick_dispatch_tasks` is repo-scoped.
///
/// `group_orphaned_routed_tasks` must select tasks whose repo != the active
/// repo, group them by repo, and leave active-repo tasks out entirely.
#[tokio::test]
async fn group_orphaned_routed_tasks_selects_inactive_repo_tasks() {
    async fn make_routed(store: &TaskStore, repo: &str, ext_id: &str) -> i64 {
        let id = store
            .upsert_external(&UpsertExternal {
                repo,
                ext_id,
                title: "Routed task",
                body: "",
                author: "user",
                url: &format!("https://github.com/{repo}/issues/{ext_id}"),
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .store_route(&StoreRoute {
                id,
                agent: "claude",
                model: Some("sonnet"),
                complexity: "medium",
                estimate: 0,
                reason: "test",
                profile: "{}",
            })
            .await
            .unwrap();
        store.update_status(id, TaskStatus::Routed).await.unwrap();
        id
    }

    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    let active_id = make_routed(&store, "active/repo", "1").await;
    let inactive_id_a = make_routed(&store, "inactive/repo", "2").await;
    let other_inactive_id = make_routed(&store, "other-inactive/repo", "3").await;
    let inactive_id_b = make_routed(&store, "inactive/repo", "4").await;

    let all_routed = store
        .list_all_by_status_global(TaskStatus::Routed)
        .await
        .unwrap();
    let active_repos: HashSet<String> = ["active/repo".to_string()].into_iter().collect();
    let by_repo = group_orphaned_routed_tasks(all_routed, &active_repos);

    assert!(
        !by_repo.contains_key("active/repo"),
        "global sweep must not select tasks for the active repo"
    );
    let inactive_ids: HashSet<i64> = by_repo
        .get("inactive/repo")
        .expect("inactive/repo must be selected")
        .iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(
        inactive_ids,
        [inactive_id_a, inactive_id_b].into_iter().collect(),
        "global sweep must group all orphaned tasks for a repo together"
    );
    assert_eq!(
        by_repo
            .get("other-inactive/repo")
            .map(|tasks| tasks.iter().map(|t| t.id).collect::<Vec<_>>()),
        Some(vec![other_inactive_id])
    );

    // Sanity: the active-repo task really was created and routed, it's just
    // excluded from the selection above.
    assert_eq!(
        store.get(active_id).await.unwrap().status,
        TaskStatus::Routed
    );
}

/// Regression test for the PR #3413 review finding: a single `current_repo` string is
/// the wrong exclusion — repo B's routed tasks must not look "orphaned" during repo A's
/// dispatch just because A != B.
/// `group_orphaned_routed_tasks` must exclude every repo in the full active set.
#[tokio::test]
async fn group_orphaned_routed_tasks_excludes_all_active_repos_not_just_current() {
    async fn make_routed(store: &TaskStore, repo: &str, ext_id: &str) -> i64 {
        let id = store
            .upsert_external(&UpsertExternal {
                repo,
                ext_id,
                title: "Routed task",
                body: "",
                author: "user",
                url: &format!("https://github.com/{repo}/issues/{ext_id}"),
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .store_route(&StoreRoute {
                id,
                agent: "claude",
                model: Some("sonnet"),
                complexity: "medium",
                estimate: 0,
                reason: "test",
                profile: "{}",
            })
            .await
            .unwrap();
        store.update_status(id, TaskStatus::Routed).await.unwrap();
        id
    }

    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    let repo_a_id = make_routed(&store, "active/repo-a", "1").await;
    let repo_b_id = make_routed(&store, "active/repo-b", "2").await;
    let orphaned_id = make_routed(&store, "inactive/repo", "3").await;

    let all_routed = store
        .list_all_by_status_global(TaskStatus::Routed)
        .await
        .unwrap();
    // Simulate repo A's tick: current repo is "active/repo-a", but the full
    // active project set also includes "active/repo-b".
    let active_repos: HashSet<String> = ["active/repo-a".to_string(), "active/repo-b".to_string()]
        .into_iter()
        .collect();
    let by_repo = group_orphaned_routed_tasks(all_routed, &active_repos);

    assert!(
        !by_repo.contains_key("active/repo-a"),
        "the current repo's own tasks must not be swept"
    );
    assert!(
        !by_repo.contains_key("active/repo-b"),
        "another active repo's tasks must not be swept during repo A's tick"
    );
    assert_eq!(
        by_repo
            .get("inactive/repo")
            .map(|tasks| tasks.iter().map(|t| t.id).collect::<Vec<_>>()),
        Some(vec![orphaned_id]),
        "a genuinely inactive repo's tasks must still be swept"
    );

    // Sanity: repo A and repo B tasks were really created and routed, just excluded.
    assert_eq!(
        store.get(repo_a_id).await.unwrap().status,
        TaskStatus::Routed
    );
    assert_eq!(
        store.get(repo_b_id).await.unwrap().status,
        TaskStatus::Routed
    );
}

/// Tasks with the `no-agent` label must be skipped by the global dispatch sweep.
#[tokio::test]
async fn global_dispatch_sweep_skips_no_agent_tasks() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    let id = store
        .upsert_external(&UpsertExternal {
            repo: "inactive/repo",
            ext_id: "99",
            title: "No-agent routed task",
            body: "",
            author: "user",
            url: "https://github.com/inactive/repo/issues/99",
            labels: &["no-agent".to_string()],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .store_route(&StoreRoute {
            id,
            agent: "claude",
            model: Some("sonnet"),
            complexity: "medium",
            estimate: 0,
            reason: "test",
            profile: "{}",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Routed).await.unwrap();

    // Safe to call the full sweep here: with the only orphaned task filtered
    // out by the no-agent label, `group_orphaned_routed_tasks` returns an
    // empty map and `dispatch_routed_tasks_global` returns before ever
    // constructing a backend or spawning a runner.
    let tmux = Arc::new(TmuxManager::new());
    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(transport));
    let semaphore = Arc::new(Semaphore::new(4));
    let (weight_tx, _weight_rx) = mpsc::channel(16);
    let dispatching = Arc::new(DashMap::new());
    let dispatch_mode = DispatchMode {
        is_degraded: false,
        healthy_agents: 1,
        threshold: 1,
    };

    let active_repos: HashSet<String> = ["active/repo".to_string()].into_iter().collect();
    dispatch_routed_tasks_global(
        &tmux,
        &capture,
        &semaphore,
        &weight_tx,
        dispatch_mode,
        &dispatching,
        &store,
        &HashMap::new(),
        &active_repos,
    )
    .await
    .unwrap();

    assert_eq!(
        store.get(id).await.unwrap().status,
        TaskStatus::Routed,
        "no-agent tasks must not be dispatched"
    );
}
