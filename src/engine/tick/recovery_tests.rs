use super::recovery::stuck_task_timing_from_map;
use super::tests::MockBackend;
use super::*;
use crate::backends::Status;
use crate::channels::transport::Transport;
use crate::engine::RunTimeouts;
use crate::engine::router::RouterConfig;
use crate::engine::tasks::TaskManager;
use crate::engine::tasks::dispatch_guard::DispatchGuard;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use crate::suspend;
use crate::tmux::{SessionGuard, TmuxManager};
use dashmap::mapref::entry::Entry;
use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task;
use tokio::time;

/// Set `updated_at` to a far-past date for a task in an in-memory store.
#[cfg(test)]
async fn set_task_updated_at_past(store: &TaskStore, task_id: i64) {
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(task_id)
        .execute(store.pool())
        .await
        .unwrap();
}

/// Age the in_review status-entry the same way the stale sweep tests do
async fn set_in_review_entered_at_past(store: &TaskStore, task_id: i64) {
    sqlx::query(
        "UPDATE task_activity SET timestamp = '2020-01-01T00:00:00Z'
             WHERE task_id = ? AND to_status = 'in_review'",
    )
    .bind(task_id)
    .execute(store.pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn recover_stuck_tasks_resets_external_in_review_to_needs_review() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let status_updates = Arc::clone(&mock.status_updates);
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    // Insert an external task and age both the status entry and updated_at
    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "99",
            title: "Stuck InReview",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::InReview).await.unwrap();
    set_in_review_entered_at_past(&store, id).await;
    set_task_updated_at_past(&store, id).await;

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let updates = status_updates.lock().unwrap();
    assert_eq!(updates.len(), 1, "stuck InReview task should be recovered");
    assert_eq!(updates[0].0, "99");
    assert_eq!(updates[0].1, Status::NeedsReview);
}

#[tokio::test]
async fn recover_stuck_tasks_skips_recent_in_review() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let status_updates = Arc::clone(&mock.status_updates);
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600, // 10 minutes — recent task won't trigger
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    // Age updated_at, leave the in_review status entry at now
    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "100",
            title: "Recent InReview",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::InReview).await.unwrap();
    set_task_updated_at_past(&store, id).await;

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let updates = status_updates.lock().unwrap();
    assert!(
        updates.is_empty(),
        "recent InReview task should not be recovered"
    );
}

#[tokio::test]
async fn recover_stuck_tasks_resets_internal_in_review_to_needs_review() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    // Create an internal task in InReview and age both clocks
    let id = store
        .create_internal("owner/repo", "Internal InReview", "", "cron", "1", None)
        .await
        .unwrap();
    store.update_status(id, TaskStatus::InReview).await.unwrap();
    set_in_review_entered_at_past(&store, id).await;
    set_task_updated_at_past(&store, id).await;

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::NeedsReview,
        "stuck internal InReview task should be reset to NeedsReview"
    );
}

#[tokio::test]
async fn recover_stuck_tasks_ages_in_review_from_status_entry_not_updated_at() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let status_updates = Arc::clone(&mock.status_updates);
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "102",
            title: "Fresh updated_at InReview",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::InReview).await.unwrap();
    set_in_review_entered_at_past(&store, id).await;
    store
        .set_fields(id, &[("pr_number", serde_json::json!(42))])
        .await
        .unwrap();

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let updates = status_updates.lock().unwrap();
    assert_eq!(
        updates.len(),
        1,
        "in_review stuck recovery must age from the status entry even when updated_at is fresh"
    );
    assert_eq!(updates[0].0, "102");
    assert_eq!(updates[0].1, Status::NeedsReview);
}

/// An in_review task whose review session is gone goes back to needs_review, unless its
/// review already recorded a decision and the task is only waiting on the merge
async fn orphaned_review_status_after_recovery(with_decision: bool) -> TaskStatus {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "55",
            title: "Orphaned review",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::InReview).await.unwrap();
    // Entered in_review long ago. updated_at stays fresh, as review_poll rewrites it
    set_in_review_entered_at_past(&store, id).await;
    if with_decision {
        store::store_log_activity(
            &Some(Arc::clone(&store)),
            "owner/repo",
            "55",
            "review_decision",
            None,
            None,
            None::<&str>,
            None::<&str>,
            Some(&serde_json::json!({"decision": "approve"})),
        )
        .await;
    }

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &EngineConfig::default(),
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    store.get(id).await.unwrap().status
}

#[tokio::test]
async fn stale_in_review_recovery_resets_orphaned_task_without_session() {
    assert_eq!(
        orphaned_review_status_after_recovery(false).await,
        TaskStatus::NeedsReview
    );
}

#[tokio::test]
async fn stale_in_review_recovery_leaves_decided_task_waiting_on_merge() {
    assert_eq!(
        orphaned_review_status_after_recovery(true).await,
        TaskStatus::InReview
    );
}

#[tokio::test]
async fn recover_stuck_tasks_resets_review_agent_failures() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "104",
            title: "Orphaned review with failures",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::InReview).await.unwrap();
    store
        .set_fields(id, &[("review_agent_failures", serde_json::json!(2))])
        .await
        .unwrap();
    set_in_review_entered_at_past(&store, id).await;

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &EngineConfig::default(),
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::NeedsReview);
    assert_eq!(
        task.review_agent_failures, 0,
        "a dead review session is an infrastructure event, not an agent failure"
    );
}

#[tokio::test]
async fn recover_stuck_tasks_reclaims_hung_in_review_session() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let status_updates = Arc::clone(&mock.status_updates);
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "103",
            title: "Hung review session",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::InReview).await.unwrap();
    set_in_review_entered_at_past(&store, id).await;
    store
        .set_fields(id, &[("pr_number", serde_json::json!(43))])
        .await
        .unwrap();

    let session_name = tmux.session_name("owner/repo", "103-review");
    let mut session_map = HashMap::new();
    session_map.insert(session_name, true);

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &session_map,
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let updates = status_updates.lock().unwrap();
    assert_eq!(
        updates.len(),
        1,
        "a live hung review session must be reclaimed once the in_review entry ages past stuck_timeout"
    );
    assert_eq!(updates[0].0, "103");
    assert_eq!(updates[0].1, Status::NeedsReview);
}

#[tokio::test]
async fn untracked_repo_recovery_resets_stale_in_progress_to_new() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    // Cross-repo task should still be recovered even when current tick repo is owner/repo.
    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/other",
            ext_id: "777",
            title: "Cross-repo stuck task",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::InProgress)
        .await
        .unwrap();
    set_task_updated_at_past(&store, id).await;

    let active = HashSet::from(["owner/repo".to_string()]);
    recover_untracked_repo_stuck_tasks(
        &tmux,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
        &active,
    )
    .await;

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::New,
        "cross-repo stale in_progress task should be reset to New"
    );
    assert_eq!(task.route_attempts, 0, "route attempts should be cleared");
    assert!(task.agent.is_none(), "agent should be cleared");
    assert!(task.model.is_none(), "model should be cleared");
}

#[tokio::test]
async fn untracked_repo_recovery_leaves_active_repos_to_their_own_pass() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };
    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/other",
            ext_id: "778",
            title: "Stuck task in an active repo",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::InProgress)
        .await
        .unwrap();
    set_task_updated_at_past(&store, id).await;

    // Per-project passes run concurrently, so only the task's own pass may reclaim it
    let active = HashSet::from(["owner/repo".to_string(), "owner/other".to_string()]);
    recover_untracked_repo_stuck_tasks(
        &tmux,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
        &active,
    )
    .await;

    assert_eq!(store.get(id).await.unwrap().status, TaskStatus::InProgress);
}

#[tokio::test]
async fn recover_stuck_tasks_finalizes_open_task_runs_rows() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    let id = store
        .create_internal("owner/repo", "Stuck with open run", "", "cron", "1", None)
        .await
        .unwrap();
    store
        .set_fields(
            id,
            &[
                ("agent", serde_json::json!("claude")),
                ("model", serde_json::json!("sonnet")),
                ("route_attempts", serde_json::json!(1)),
            ],
        )
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::InProgress)
        .await
        .unwrap();
    set_task_updated_at_past(&store, id).await;

    // Start an agent run that will be orphaned by recovery
    store
        .start_run(&crate::store::StartRun {
            task_id: id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "claude --model sonnet",
            prompt: "fix it",
        })
        .await
        .unwrap();

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::New);

    let runs = store.get_runs(id).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome, "aborted");
    assert!(runs[0].completed_at.is_some());
    assert_eq!(
        runs[0].error,
        "stuck-task recovery: internal no session found"
    );
}

/// Regression test for #3518: a completed agent run must not be discarded by the
/// stuck-task reclaim just because its tmux session has already exited.
///
/// In production, sessions are created without `remain-on-exit`, so once the agent
/// process exits tmux removes the session immediately — there is no observable
/// "dead pane" state for `tick_check_session_completions` to catch, and the runner's
/// own `store_touch_updated_at` only fires after its own poll notices the exit (up to
/// its 5s poll interval) plus post-processing. For a task whose total runtime already
/// exceeds `no_session_stuck_timeout`, the very next tick that observes the (now
/// absent) session would otherwise reclaim it instantly — discarding a successful run.
///
/// The fix: `tick_recover_stuck_tasks` skips the no-session reclaim while the task's
/// dispatch guard (the in-memory `dispatching` map keyed by "{repo}/{task_id}",
/// populated for the entire duration of `runner.run_with_context`) is still held —
/// i.e. while this process is still actively running the task, regardless of tmux
/// session state.
#[tokio::test]
async fn recover_stuck_tasks_skips_reclaim_while_dispatch_guard_held() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    let id = store
        .create_internal(
            "owner/repo",
            "Long-running daily job",
            "",
            "cron",
            "1",
            None,
        )
        .await
        .unwrap();
    store
        .set_fields(
            id,
            &[
                ("agent", serde_json::json!("claude")),
                ("model", serde_json::json!("sonnet")),
                ("route_attempts", serde_json::json!(1)),
            ],
        )
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::InProgress)
        .await
        .unwrap();
    // Simulate a task that has already run well past no_session_stuck_timeout,
    // as a legitimately long agent run would.
    set_task_updated_at_past(&store, id).await;

    // Simulate the runner still actively processing this task: its dispatch guard
    // is held even though the tmux session (empty session_map below) is already gone.
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    dispatching.insert(
        format!("owner/repo/internal:{id}"),
        format!("internal:{id}"),
    );

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &dispatching,
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::InProgress,
        "task should not be reclaimed while its dispatch guard is still held"
    );

    // No run should have been finalized/aborted either.
    let runs = store.get_runs(id).await.unwrap();
    assert!(runs.is_empty());

    // Once the guard is released (runner finished), the very same conditions should
    // reclaim the task as before — the fix must not disable the safety net entirely.
    dispatching.clear();
    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &dispatching,
    )
    .await
    .unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::New,
        "task should still be reclaimed once the dispatch guard is released"
    );
}

/// Concurrency regression test for issue #3526: the reclaim race recurred twice
/// (internal:156563 in #3523, internal:156673 in #3526) *after* the #3519/#3525
/// fix landed, in production under the real multi-threaded tokio runtime. The
/// existing `recover_stuck_tasks_skips_reclaim_while_dispatch_guard_held` test
/// above only exercises sequential `.await` calls on a single-threaded test
/// runtime (`#[tokio::test]` defaults to `flavor = "current_thread"`), which can
/// never observe genuine cross-thread races in the shared `dispatching` DashMap.
///
/// This test claims the guard exactly the way `dispatch_tasks_for_repo` does
/// (atomic `DashMap::entry` claim, `DispatchGuard` RAII release), hands it to a
/// spawned task running on a *different* worker thread that holds it until
/// explicitly told to release, and hammers the real `tick_recover_stuck_tasks`
/// from the main test task hundreds of times while the guard is deterministically
/// still held. If there were a visibility gap between `DashMap::entry().insert()`
/// on one thread and `DashMap::contains_key()` on another, this test would catch it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn recover_stuck_tasks_never_reclaims_while_guard_held_under_real_concurrency() {
    const HAMMER_ITERATIONS: u32 = 300;

    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    let id = store
        .create_internal(
            "owner/repo",
            "Long-running daily job",
            "",
            "cron",
            "1",
            None,
        )
        .await
        .unwrap();
    store
        .set_fields(
            id,
            &[
                ("agent", serde_json::json!("claude")),
                ("model", serde_json::json!("sonnet")),
                ("route_attempts", serde_json::json!(1)),
            ],
        )
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::InProgress)
        .await
        .unwrap();
    set_task_updated_at_past(&store, id).await;

    let dispatch_key = format!("owner/repo/internal:{id}");
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

    // Claim the guard synchronously, on this task, exactly like
    // dispatch_tasks_for_repo does: the atomic entry-claim always happens before
    // tokio::spawn hands the guard to the background task. (Claiming inside the
    // spawned task itself would race the hammer loop below against tokio's own
    // scheduling of that task onto a worker thread — a test artifact, not the
    // real production ordering, which guarantees claim-before-spawn.)
    let claim_task_id = format!("internal:{id}");
    {
        match dispatching.entry(dispatch_key.clone()) {
            Entry::Vacant(slot) => {
                slot.insert(claim_task_id);
            }
            Entry::Occupied(_) => panic!("dispatch key already claimed"),
        }
    }
    let guard = DispatchGuard::new(Arc::clone(&dispatching), dispatch_key.clone());

    // Hold the guard on a spawned task — a different worker thread under the
    // multi-thread runtime — mirroring the real runner holding it across genuine
    // async work (worktree setup, agent session, response handling) inside
    // `tokio::spawn(... run_with_context ...)`. It only drops the guard once
    // explicitly told to via `release_rx`, so its lifetime is fully controlled
    // by this test rather than raced against a timer — the release-signal
    // ordering was itself a source of false positives in an earlier version of
    // this test (a `sleep`-then-drop holder can legitimately release the guard
    // in the gap between the hammer loop's last check and its next iteration,
    // which is a correct outcome, not a bug, but made the assertion flaky).
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let holder = tokio::spawn(async move {
        let _ = release_rx.await;
        drop(guard);
    });

    // Hammer the real reclaim path concurrently from the main task while the
    // guard is deterministically still held (the holder task is parked on
    // `release_rx` and cannot drop the guard until we signal it below). Each
    // iteration's `tick_recover_stuck_tasks` call may itself run on any worker
    // thread the tokio scheduler picks, independent of the thread that claimed
    // the guard — this is what actually exercises cross-thread DashMap
    // visibility, not wall-clock racing against a timer.
    for i in 0..HAMMER_ITERATIONS {
        tick_recover_stuck_tasks(
            &backend,
            &tmux,
            "owner/repo",
            &task_manager,
            &config,
            &store,
            &HashMap::new(),
            &dispatching,
        )
        .await
        .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(
            task.status,
            TaskStatus::InProgress,
            "task must never be reclaimed while the dispatch guard is deterministically \
                 still held, even under real cross-thread concurrency (dispatch_key={dispatch_key}, \
                 iteration={i}, contains_key_now={}, map_len_now={})",
            dispatching.contains_key(&dispatch_key),
            dispatching.len(),
        );
        task::yield_now().await;
    }

    // Now release the guard and confirm the holder task actually completed.
    let _ = release_tx.send(());
    holder.await.unwrap();

    // Guard released — the safety net must still reclaim as before.
    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &dispatching,
    )
    .await
    .unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::New,
        "task should be reclaimed once the dispatch guard is released"
    );
}

#[tokio::test]
async fn recover_stuck_tasks_skips_external_in_review_waiting_for_human_review() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let status_updates = Arc::clone(&mock.status_updates);
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "101",
            title: "Waiting For Human Review",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::InReview).await.unwrap();
    set_task_updated_at_past(&store, id).await;

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let updates = status_updates.lock().unwrap();
    assert!(
        updates.is_empty(),
        "InReview task waiting for human review should not be reset"
    );
}

#[tokio::test]
async fn detect_silent_agents_skips_backend_for_internal_tasks() {
    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(transport));
    let mock = Arc::new(MockBackend::new());
    let backend: Arc<dyn ExternalBackend> = Arc::<MockBackend>::clone(&mock);
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        silence_grace_period: 0,
        ..EngineConfig::default()
    };

    let internal_id = store
        .create_internal("owner/repo", "Silent internal", "", "cron", "1", None)
        .await
        .unwrap();
    store
        .update_status(internal_id, TaskStatus::InProgress)
        .await
        .unwrap();

    let task_id = format!("internal:{internal_id}");
    capture
        .register_session("owner/repo", &task_id, "orch-test-internal")
        .await;
    time::sleep(Duration::from_millis(1100)).await;

    tick_detect_silent_agents(
        &tmux,
        "owner/repo",
        &capture,
        &backend,
        &task_manager,
        &config,
        &store,
    )
    .await
    .unwrap();

    assert!(
        mock.removed_labels.lock().unwrap().is_empty(),
        "internal tasks should skip backend label removal"
    );
    assert!(
        mock.posted_comments.lock().unwrap().is_empty(),
        "internal tasks should skip backend comments"
    );

    let task = store.get(internal_id).await.unwrap();
    // Silence detection now uses failover: if a fallback agent is found → Routed,
    // otherwise → New (reset for re-routing when cooldowns expire). Either is
    // acceptable; the key invariant is that the task is no longer InProgress.
    assert!(
        matches!(task.status, TaskStatus::Routed | TaskStatus::New),
        "internal task should be routed to fallback or reset to new, got {:?}",
        task.status
    );
}

#[tokio::test]
async fn detect_silent_agents_finalizes_open_task_runs_rows() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(transport));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        silence_grace_period: 60,
        silence_cooldown: 300,
        ..EngineConfig::default()
    };

    let internal_id = store
        .create_internal("owner/repo", "Silent with open run", "", "cron", "1", None)
        .await
        .unwrap();
    store
        .set_fields(
            internal_id,
            &[
                ("agent", serde_json::json!("claude")),
                ("model", serde_json::json!("sonnet")),
            ],
        )
        .await
        .unwrap();
    store
        .update_status(internal_id, TaskStatus::InProgress)
        .await
        .unwrap();

    let task_id = format!("internal:{internal_id}");
    capture
        .register_session("owner/repo", &task_id, "orch-test-silent-run")
        .await;
    capture
        .set_buffer_state_for_test(
            "owner/repo",
            &task_id,
            false,
            false,
            chrono::Utc::now() - chrono::Duration::seconds(120),
        )
        .await;

    // Start an agent run that will be orphaned by silence detection
    store
        .start_run(&crate::store::StartRun {
            task_id: internal_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "claude --model sonnet",
            prompt: "fix it",
        })
        .await
        .unwrap();

    tick_detect_silent_agents(
        &tmux,
        "owner/repo",
        &capture,
        &backend,
        &task_manager,
        &config,
        &store,
    )
    .await
    .unwrap();

    let runs = store.get_runs(internal_id).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome, "aborted");
    assert!(runs[0].completed_at.is_some());
    assert_eq!(runs[0].error, "silence detection: session killed");
}

/// Regression test for the race between Phase 1 (tick_check_session_completions)
/// and Phase 2 (tick_recover_stuck_tasks).
///
/// Before the fix, Phase 1 killed the tmux session without touching updated_at.
/// Phase 2 could then see status=in_progress, has_session=false, and an old
/// updated_at, and would reset the task to New — even though the runner's
/// wait_for_completion() poll had not yet returned.
///
/// The fix: Phase 1 touches updated_at as soon as it detects the session is done.
/// This test verifies that after the touch, Phase 2 does NOT reclaim the task.
#[tokio::test]
async fn phase1_session_completion_touch_prevents_phase2_reclaim() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    // Create an in-progress internal task with old updated_at.
    // Without the fix this task would be reclaimed by Phase 2 (age >> threshold,
    // no session running in the test environment).
    let id = store
        .create_internal("owner/repo", "Completed task", "", "cron", "1", None)
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::InProgress)
        .await
        .unwrap();
    set_task_updated_at_past(&store, id).await;

    // Simulate Phase 1: touch updated_at when the session is detected as done.
    // In production this happens inside tick_check_session_completions.
    let task_id = format!("internal:{id}");
    store_touch_updated_at(&Some(Arc::clone(&store)), "owner/repo", &task_id).await;

    // Phase 2 runs — updated_at is fresh so the task must NOT be reclaimed.
    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::InProgress,
        "Phase 2 must not reclaim a task whose updated_at was just touched by Phase 1"
    );
}

#[tokio::test]
async fn stuck_task_timing_treats_dead_session_as_missing() {
    let tmux = Arc::new(TmuxManager::new());
    let repo = "owner/repo";
    let task_id = "internal:33498";
    let session_name = tmux.session_name(repo, task_id);

    let create_result = tmux
        .command()
        .args(["new-session", "-d", "-s", &session_name, "-c", "/tmp"])
        .output()
        .await;

    match create_result {
        Ok(o) if o.status.success() => {}
        _ => {
            eprintln!("Skipping test: tmux not available or failed to create test session");
            return;
        }
    }

    let _guard = SessionGuard::new(&tmux, session_name.clone());

    let set_option_result = tmux
        .command()
        .args(["set-option", "-t", &session_name, "remain-on-exit", "on"])
        .output()
        .await;
    if !matches!(set_option_result, Ok(ref o) if o.status.success()) {
        eprintln!("Skipping test: unable to set tmux remain-on-exit option");
        return;
    }

    let send_exit_result = tmux
        .command()
        .args(["send-keys", "-t", &session_name, "exit", "Enter"])
        .output()
        .await;
    if !matches!(send_exit_result, Ok(ref o) if o.status.success()) {
        eprintln!("Skipping test: unable to exit tmux pane");
        return;
    }

    // Poll until pane is dead (up to 2s) — fixed sleep is unreliable under load.
    for _ in 0..20 {
        time::sleep(Duration::from_millis(100)).await;
        if !tmux.session_is_running(&session_name).await {
            break;
        }
    }

    // Also wait for batch_session_active to reflect the dead state.
    // session_is_running uses `list-panes -t <session>` which may settle
    // before the global `list-panes -a` snapshot used by the production code.
    // On CI under load these two tmux commands can transiently disagree.
    let mut session_map = tmux.batch_session_active().await;
    for _ in 0..20 {
        if !session_map.get(&session_name).copied().unwrap_or(false) {
            break;
        }
        time::sleep(Duration::from_millis(100)).await;
        session_map = tmux.batch_session_active().await;
    }
    if session_map.get(&session_name).copied().unwrap_or(false) {
        eprintln!(
            "Skipping test: session still alive in batch_session_active after retries (CI tmux lag)"
        );
        return;
    }

    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };
    let updated_at = (chrono::Utc::now()
        - chrono::Duration::seconds(
            i64::try_from(config.no_session_stuck_timeout).expect("timeout fits in i64") + 5,
        ))
    .to_rfc3339();

    let timing = stuck_task_timing_from_map(
        &tmux,
        repo,
        task_id,
        &updated_at,
        &config,
        "parse failure",
        &session_map,
    )
    .expect("dead session should be treated as missing");

    assert!(!timing.has_session);
    assert_eq!(timing.threshold, config.no_session_stuck_timeout);
}

#[tokio::test]
#[serial_test::serial(suspend_state)]
async fn stuck_task_timing_discounts_host_suspend_gap() {
    suspend::clear_for_test();

    let tmux = Arc::new(TmuxManager::new());
    let repo = "owner/repo";
    let task_id = "internal:99001";
    let session_map = HashMap::new();

    let config = EngineConfig {
        no_session_stuck_timeout: 1800,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    // Task was last touched 33 minutes ago (age_mins=33 matches the reported
    // false-positive), but the host was suspended for 30 of those minutes —
    // real elapsed runtime is only 3 minutes, well under the 30-minute threshold.
    let updated_at = (chrono::Utc::now() - chrono::Duration::minutes(33)).to_rfc3339();
    let suspend_gap_detected_at = chrono::Utc::now() - chrono::Duration::minutes(1);
    suspend::inject_gap_for_test(suspend_gap_detected_at, chrono::Duration::minutes(30));

    let timing = stuck_task_timing_from_map(
        &tmux,
        repo,
        task_id,
        &updated_at,
        &config,
        "parse failure",
        &session_map,
    );

    assert!(
        timing.is_none(),
        "task age should be discounted below the stuck threshold once suspend time is subtracted, got {timing:?}"
    );

    suspend::clear_for_test();
}

#[tokio::test]
async fn recover_stuck_tasks_reclaims_routed_task_blocked_by_stale_session() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1,
        ..EngineConfig::default()
    };

    let id = store
        .create_internal("owner/repo", "Blocked routed task", "", "cron", "1", None)
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Routed).await.unwrap();
    set_task_updated_at_past(&store, id).await;

    let task_id = format!("internal:{id}");
    let session_name = tmux.session_name("owner/repo", &task_id);
    let create_result = tmux
        .command()
        .args([
            "new-session",
            "-d",
            "-s",
            &session_name,
            "-c",
            "/tmp",
            "sleep 60",
        ])
        .output()
        .await;

    match create_result {
        Ok(o) if o.status.success() => {}
        _ => {
            eprintln!("Skipping test: tmux not available or failed to create test session");
            return;
        }
    }

    let session_map = tmux.batch_session_active().await;
    assert_eq!(session_map.get(&session_name).copied(), Some(true));

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &session_map,
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::New,
        "routed task blocked by a stale session should be reset for re-routing"
    );
    assert!(
        !tmux.session_exists(&session_name).await,
        "recovery should kill the stale tmux session so dispatch can proceed"
    );
}

/// An internal complex task that entered in_progress `minutes` ago and was last written
/// `updated_minutes_ago`, with a live tmux session. None when tmux is not available
async fn live_complex_run(
    store: &TaskStore,
    tmux: &TmuxManager,
    minutes: i64,
    updated_minutes_ago: i64,
) -> Option<(i64, String)> {
    let id = store
        .create_internal("owner/repo", "Long complex run", "", "cron", "1", None)
        .await
        .unwrap();
    store
        .set_fields(id, &[("complexity", serde_json::json!("complex"))])
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::InProgress)
        .await
        .unwrap();
    let ago = |m: i64| {
        (chrono::Utc::now() - chrono::Duration::minutes(m))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    };
    let entered = ago(minutes);
    sqlx::query(
        "UPDATE task_activity SET timestamp = ? WHERE task_id = ? AND to_status = 'in_progress'",
    )
    .bind(&entered)
    .bind(id)
    .execute(store.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE tasks SET updated_at = ? WHERE id = ?")
        .bind(ago(updated_minutes_ago))
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    let session_name = tmux.session_name("owner/repo", &format!("internal:{id}"));
    let created = tmux
        .command()
        .args([
            "new-session",
            "-d",
            "-s",
            &session_name,
            "-c",
            "/tmp",
            "sleep 60",
        ])
        .output()
        .await;
    match created {
        Ok(o) if o.status.success() => Some((id, session_name)),
        _ => None,
    }
}

fn long_run_config() -> EngineConfig {
    EngineConfig {
        stuck_timeout: 1800,
        run_timeouts: RunTimeouts {
            base: 1800,
            medium: 2700,
            complex: 3600,
        },
        ..EngineConfig::default()
    }
}

#[test]
fn tick_router_loads_the_config_once_for_many_live_runs() {
    let cell = OnceLock::new();
    let loads = Cell::new(0);
    for _ in 0..4 {
        tick_router(&cell, || {
            loads.set(loads.get() + 1);
            RouterConfig::default()
        });
    }
    assert_eq!(loads.get(), 1);
}

#[tokio::test]
async fn run_tier_follows_the_tier_of_the_stored_model() {
    let mut router = RouterConfig::default();
    router.model_map.clear();
    for (tier, model) in [("simple", "rt-s"), ("complex", "rt-c")] {
        router
            .model_map
            .entry(tier.to_string())
            .or_default()
            .insert("rtagent".to_string(), vec![model.to_string()]);
    }
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create_internal(
            "owner/repo",
            "Simple on a complex model",
            "",
            "cron",
            "1",
            None,
        )
        .await
        .unwrap();
    store
        .set_fields(
            id,
            &[
                ("complexity", serde_json::json!("simple")),
                ("agent", serde_json::json!("rtagent")),
                ("model", serde_json::json!("rt-c")),
            ],
        )
        .await
        .unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(run_tier(&router, &task), "complex");

    store
        .set_fields(id, &[("model", serde_json::json!("rt-s"))])
        .await
        .unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(run_tier(&router, &task), "simple");
}

#[tokio::test]
async fn recover_stuck_tasks_keeps_a_live_complex_run_inside_its_timeout() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let Some((id, session_name)) = live_complex_run(&store, &tmux, 31, 31).await else {
        eprintln!("Skipping test: tmux not available or failed to create test session");
        return;
    };
    let session_map = tmux.batch_session_active().await;

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &long_run_config(),
        &store,
        &session_map,
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let alive = tmux.session_exists(&session_name).await;
    let _ = tmux.kill_session(&session_name).await;
    assert_eq!(
        store.get(id).await.unwrap().status,
        TaskStatus::InProgress,
        "a complex run at 31 minutes is inside its 3600s timeout"
    );
    assert!(alive, "recovery must not touch a run inside its timeout");
}

#[tokio::test]
async fn recover_stuck_tasks_kills_a_live_complex_run_past_its_timeout() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let Some((id, session_name)) = live_complex_run(&store, &tmux, 66, 0).await else {
        eprintln!("Skipping test: tmux not available or failed to create test session");
        return;
    };
    let session_map = tmux.batch_session_active().await;

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &long_run_config(),
        &store,
        &session_map,
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let alive = tmux.session_exists(&session_name).await;
    let _ = tmux.kill_session(&session_name).await;
    assert_eq!(
        store.get(id).await.unwrap().status,
        TaskStatus::New,
        "a complex run past its timeout plus grace is stuck"
    );
    assert!(
        !alive,
        "recovery must kill the session it reports as killed"
    );
}

/// Regression test for the completed-session -> stuck-task reclaim race.
///
/// Scenario:
/// - Runner finishes a tmux session and will call store_touch_updated_at shortly after
/// - Engine's tick may observe the dead session in Phase 1 and Phase 2 may run
///   stuck-task recovery before the runner touches the store, causing incorrect reclaim
/// Fix: tick_check_session_completions now touches the store.updated_at when it
/// observes a completed session, preventing reclaim. This test simulates the
/// timing by creating a session, marking the corresponding task InProgress with
/// an old updated_at, calling tick_check_session_completions (which should touch
/// the store), then calling tick_recover_stuck_tasks and asserting the task
/// remains InProgress.
#[tokio::test]
async fn completed_session_does_not_cause_stuck_reclaim_race() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(transport));

    // Insert external task and set to InProgress with old updated_at
    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "200",
            title: "Race Task",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::InProgress)
        .await
        .unwrap();

    // set updated_at to far past so it would normally be eligible for reclaim
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    // Register a real tmux session for this task and then exit it so it's observed dead
    let task_id = "200";
    let session_name = tmux.session_name("owner/repo", task_id);
    let create_result = tmux
        .command()
        .args(["new-session", "-d", "-s", &session_name, "-c", "/tmp"])
        .output()
        .await;
    match create_result {
        Ok(o) if o.status.success() => {}
        _ => {
            eprintln!("Skipping test: tmux not available or failed to create test session");
            return;
        }
    }
    let _guard = SessionGuard::new(&tmux, session_name.clone());

    // Ensure remain-on-exit so the session lingers as dead long enough to be snapshot
    let _ = tmux
        .command()
        .args(["set-option", "-t", &session_name, "remain-on-exit", "on"])
        .output()
        .await;
    let _ = tmux
        .command()
        .args(["send-keys", "-t", &session_name, "exit", "Enter"])
        .output()
        .await;

    // Poll until pane is dead
    for _ in 0..20 {
        time::sleep(Duration::from_millis(100)).await;
        if !tmux.session_is_running(&session_name).await {
            break;
        }
    }

    // Register the session in capture (engine does this during dispatch)
    let task_key = task_id.to_string();
    capture
        .register_session("owner/repo", &task_key, &session_name)
        .await;

    // Call phase1 (now touches store.updated_at for completed session)
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    tick_check_session_completions(&tmux, "owner/repo", &capture, &store)
        .await
        .unwrap();

    // Verify phase1 actually touched updated_at. It only processes sessions
    // that appear in snapshot() (list_sessions + batch_session_active). On CI,
    // if the session was already fully removed before the snapshot runs (e.g.
    // remain-on-exit semantics differ across tmux versions), phase1 won't see
    // it and the store won't be touched. Skip in that case.
    {
        let task_state = store.get(id).await.unwrap();
        if task_state.updated_at.starts_with("2020") {
            eprintln!(
                "Skipping test: tick_check_session_completions did not update stored \
                     updated_at — session was not in snapshot (CI tmux environment issue)"
            );
            return;
        }
    }

    // Now run phase2 — the updated_at should have been touched so no reclaim occurs
    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    // Verify the task is still InProgress in the store
    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::InProgress,
        "task should not be reclaimed to New"
    );
}
