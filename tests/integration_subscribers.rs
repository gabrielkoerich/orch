//! Integration tests for the four event subscriber handlers.
//!
//! Each subscriber reacts to [`TaskEvent`] broadcasts on a specific status:
//!
//! | Subscriber | Trigger status | Action |
//! |------------|----------------|--------|
//! | dispatch   | `routed`       | call `tick_dispatch_tasks` immediately |
//! | notify     | terminal states| push `TaskNotification` to transport |
//! | review     | `needs_review` | spawn review agent |
//! | unblock    | `done`         | call `tick_unblock_parents` immediately |
//!
//! Run:
//! ```bash
//! cargo nextest run --test integration_subscribers
//! ```

use async_trait::async_trait;
use dashmap::DashSet;
use orch::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use orch::channels::capture::CaptureService;
use orch::channels::transport::Transport;
use orch::engine::events::TaskEvent;
use orch::engine::router::{Router, RouterConfig};
use orch::engine::runner::{TaskRunner, WeightSignal};
use orch::engine::subscribers::{dispatch, notify, review, unblock};
use orch::engine::tasks::TaskManager;
use orch::store::TaskStore;
use orch::tmux::TmuxManager;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock, Semaphore};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Minimal mock backend — passes health checks, returns empty lists.
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

/// Build a [`TaskEvent`] with the given task_id, repo, and new_status.
fn make_event(task_id: &str, repo: &str, new_status: &str) -> TaskEvent {
    TaskEvent {
        task_id: task_id.to_string(),
        repo: repo.to_string(),
        old_status: "new".to_string(),
        new_status: new_status.to_string(),
        agent: Some("claude".to_string()),
        model: None,
        pr_number: None,
        branch: None,
        review_context: None,
        error: None,
        timestamp: "2026-01-01T00:00:00Z".to_string(),
        title: Some("Test task title".to_string()),
        summary: Some("Test summary".to_string()),
        duration_seconds: Some(42.0),
    }
}

/// Return a unique temp-DB path to avoid collisions when tests run in parallel.
fn temp_db(label: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("orch-sub-{label}-{}-{n}.db", std::process::id()))
}

fn cleanup_db(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
}

// ---------------------------------------------------------------------------
// notify subscriber tests
//
// The notify subscriber filters events by NotificationLevel (default: All)
// and calls transport.push_notification() for terminal/meaningful statuses.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn notify_done_event_pushes_notification() {
    let (tx, rx) = broadcast::channel::<TaskEvent>(16);
    let transport = Arc::new(Transport::new());
    let mut notify_rx = transport.subscribe_notifications();

    notify::spawn(rx, transport.clone());

    tx.send(make_event("42", "owner/repo", "done")).unwrap();

    let notification =
        tokio::time::timeout(std::time::Duration::from_millis(300), notify_rx.recv())
            .await
            .expect("timed out waiting for notification")
            .expect("notification channel closed");

    assert_eq!(notification.task_id, "42");
    assert_eq!(notification.status, "done");
    assert_eq!(notification.title, "Test task title");
    assert_eq!(notification.agent, "claude");
    assert_eq!(notification.duration_seconds, 42.0);
    assert_eq!(notification.summary, "Test summary");
    assert_eq!(notification.repo, Some("owner/repo".to_string()));
    // Drop sender to close channel; subscriber exits cleanly.
    drop(tx);
}

#[tokio::test]
async fn notify_needs_review_event_pushes_notification() {
    let (tx, rx) = broadcast::channel::<TaskEvent>(16);
    let transport = Arc::new(Transport::new());
    let mut notify_rx = transport.subscribe_notifications();

    notify::spawn(rx, transport.clone());

    tx.send(make_event("7", "owner/repo", "needs_review"))
        .unwrap();

    let notification =
        tokio::time::timeout(std::time::Duration::from_millis(300), notify_rx.recv())
            .await
            .expect("timeout")
            .expect("closed");

    assert_eq!(notification.task_id, "7");
    assert_eq!(notification.status, "needs_review");
    drop(tx);
}

#[tokio::test]
async fn notify_blocked_event_pushes_notification() {
    let (tx, rx) = broadcast::channel::<TaskEvent>(16);
    let transport = Arc::new(Transport::new());
    let mut notify_rx = transport.subscribe_notifications();

    notify::spawn(rx, transport.clone());

    tx.send(make_event("3", "owner/repo", "blocked")).unwrap();

    let notification =
        tokio::time::timeout(std::time::Duration::from_millis(300), notify_rx.recv())
            .await
            .expect("timeout")
            .expect("closed");

    assert_eq!(notification.status, "blocked");
    drop(tx);
}

#[tokio::test]
async fn notify_failed_event_pushes_notification() {
    let (tx, rx) = broadcast::channel::<TaskEvent>(16);
    let transport = Arc::new(Transport::new());
    let mut notify_rx = transport.subscribe_notifications();

    notify::spawn(rx, transport.clone());

    tx.send(make_event("8", "owner/repo", "failed")).unwrap();

    let notification =
        tokio::time::timeout(std::time::Duration::from_millis(300), notify_rx.recv())
            .await
            .expect("timeout")
            .expect("closed");

    assert_eq!(notification.status, "failed");
    drop(tx);
}

/// The default notification level is `All`, which suppresses intermediate
/// transitions: new, routed, in_progress, in_review.
#[tokio::test]
async fn notify_intermediate_statuses_not_forwarded() {
    let (tx, rx) = broadcast::channel::<TaskEvent>(16);
    let transport = Arc::new(Transport::new());
    let mut notify_rx = transport.subscribe_notifications();

    notify::spawn(rx, transport.clone());

    for status in &["new", "routed", "in_progress", "in_review"] {
        tx.send(make_event("1", "owner/repo", status)).unwrap();
    }

    // None of these should produce a notification
    let result =
        tokio::time::timeout(std::time::Duration::from_millis(150), notify_rx.recv()).await;

    assert!(
        result.is_err(),
        "intermediate status events should not produce notifications"
    );
}

/// Notifications from multiple terminal events all reach the transport.
#[tokio::test]
async fn notify_multiple_terminal_events_all_forwarded() {
    let (tx, rx) = broadcast::channel::<TaskEvent>(16);
    let transport = Arc::new(Transport::new());
    let mut notify_rx = transport.subscribe_notifications();

    notify::spawn(rx, transport.clone());

    tx.send(make_event("1", "repo/a", "done")).unwrap();
    tx.send(make_event("2", "repo/b", "blocked")).unwrap();
    tx.send(make_event("3", "repo/c", "needs_review")).unwrap();

    let mut received_ids: Vec<String> = Vec::new();
    for _ in 0..3 {
        let n = tokio::time::timeout(std::time::Duration::from_millis(300), notify_rx.recv())
            .await
            .expect("timeout waiting for notification")
            .expect("channel closed");
        received_ids.push(n.task_id.clone());
    }

    assert!(
        received_ids.contains(&"1".to_string()),
        "task 1 not notified"
    );
    assert!(
        received_ids.contains(&"2".to_string()),
        "task 2 not notified"
    );
    assert!(
        received_ids.contains(&"3".to_string()),
        "task 3 not notified"
    );
}

/// Dropping the sender closes the broadcast channel; the subscriber should
/// exit its loop cleanly rather than panicking.
#[tokio::test]
async fn notify_channel_close_exits_cleanly() {
    let (tx, rx) = broadcast::channel::<TaskEvent>(16);
    let transport = Arc::new(Transport::new());

    notify::spawn(rx, transport.clone());

    drop(tx); // close the channel
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // Reaches here without panic — test passes
}

// ---------------------------------------------------------------------------
// dispatch subscriber tests
//
// The dispatch subscriber reacts to `routed` events for its configured repo.
// Matching events call tick_dispatch_tasks; with an empty store the call is
// a no-op. Non-matching events (wrong status or wrong repo) are ignored.
// ---------------------------------------------------------------------------

/// Construct a minimal dispatch subscriber and return the channel sender,
/// the dispatching set (for guard inspection), and the temp DB path.
async fn make_dispatch_harness(
    repo: &str,
) -> (
    broadcast::Sender<TaskEvent>,
    Arc<DashSet<String>>,
    std::path::PathBuf,
) {
    let tmp = temp_db(&format!("dispatch-{}", repo.replace('/', "-")));
    let store = Arc::new(TaskStore::open_single(&tmp).await.expect("open store"));
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend);
    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(transport));
    let tmux = Arc::new(TmuxManager::new());
    let runner = Arc::new(TaskRunner::new(repo.to_string()).with_store(store.clone()));
    let semaphore = Arc::new(Semaphore::new(4));
    let task_manager = Arc::new(TaskManager::with_store(
        backend.clone(),
        store.clone(),
        repo.to_string(),
    ));
    let (weight_tx, _weight_rx) = mpsc::channel::<WeightSignal>(16);
    // Empty agent list: router has nothing to dispatch to — safe for tests.
    let config = RouterConfig {
        mode: "round_robin".to_string(),
        agents: vec![],
        ..RouterConfig::default()
    };
    let router = Arc::new(RwLock::new(Router::new(config)));
    let dispatching = Arc::new(DashSet::<String>::new());

    let (tx, rx) = broadcast::channel::<TaskEvent>(16);
    dispatch::spawn(
        rx,
        backend,
        tmux,
        runner,
        capture,
        semaphore,
        task_manager,
        weight_tx,
        router,
        dispatching.clone(),
        store,
        repo.to_string(),
    );

    (tx, dispatching, tmp)
}

#[tokio::test]
async fn dispatch_wrong_repo_event_ignored() {
    let (tx, _dispatching, tmp) = make_dispatch_harness("owner/repo").await;

    // Event for a different repo — subscriber should silently skip it.
    tx.send(make_event("42", "other/repo", "routed")).unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    cleanup_db(&tmp);
}

#[tokio::test]
async fn dispatch_wrong_status_events_ignored() {
    let (tx, _dispatching, tmp) = make_dispatch_harness("owner/repo").await;

    // None of these statuses match `routed` — all should be ignored.
    for status in &["done", "in_progress", "needs_review", "blocked", "new"] {
        tx.send(make_event("10", "owner/repo", status)).unwrap();
    }

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    cleanup_db(&tmp);
}

/// A `routed` event for the right repo causes tick_dispatch_tasks to run.
/// With an empty store and no available agents, the tick returns immediately
/// without dispatching — verifies the subscriber doesn't panic on the happy path.
#[tokio::test]
async fn dispatch_matching_event_does_not_panic_with_empty_store() {
    let (tx, _dispatching, tmp) = make_dispatch_harness("owner/repo").await;

    tx.send(make_event("42", "owner/repo", "routed")).unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    // No panic — test passes by reaching here.
    cleanup_db(&tmp);
}

#[tokio::test]
async fn dispatch_channel_close_exits_cleanly() {
    let (tx, _dispatching, tmp) = make_dispatch_harness("test/repo").await;
    drop(tx);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    cleanup_db(&tmp);
}

// ---------------------------------------------------------------------------
// unblock subscriber tests
//
// The unblock subscriber reacts to `done` events for its configured repo,
// calling tick_unblock_parents which is a no-op with an empty store.
// ---------------------------------------------------------------------------

async fn make_unblock_harness(repo: &str) -> (broadcast::Sender<TaskEvent>, std::path::PathBuf) {
    let tmp = temp_db(&format!("unblock-{}", repo.replace('/', "-")));
    let store = Arc::new(TaskStore::open_single(&tmp).await.expect("open store"));
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend);
    let task_manager = Arc::new(TaskManager::with_store(
        backend.clone(),
        store.clone(),
        repo.to_string(),
    ));

    let (tx, rx) = broadcast::channel::<TaskEvent>(16);
    unblock::spawn(rx, backend, task_manager, store, repo.to_string());

    (tx, tmp)
}

#[tokio::test]
async fn unblock_wrong_repo_event_ignored() {
    let (tx, tmp) = make_unblock_harness("owner/repo").await;

    tx.send(make_event("10", "other/repo", "done")).unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    cleanup_db(&tmp);
}

#[tokio::test]
async fn unblock_wrong_status_events_ignored() {
    let (tx, tmp) = make_unblock_harness("owner/repo").await;

    // Only `done` triggers unblock; all other statuses are ignored.
    for status in &["routed", "in_progress", "needs_review", "blocked", "new"] {
        tx.send(make_event("10", "owner/repo", status)).unwrap();
    }

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    cleanup_db(&tmp);
}

/// A `done` event for the right repo calls tick_unblock_parents.
/// With an empty store there are no blocked parents to unblock — no panic.
#[tokio::test]
async fn unblock_done_event_right_repo_does_not_panic() {
    let (tx, tmp) = make_unblock_harness("owner/repo").await;

    tx.send(make_event("10", "owner/repo", "done")).unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    cleanup_db(&tmp);
}

#[tokio::test]
async fn unblock_channel_close_exits_cleanly() {
    let (tx, tmp) = make_unblock_harness("test/repo").await;
    drop(tx);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    cleanup_db(&tmp);
}

/// Two unblock subscribers for different repos, sharing the same broadcast
/// sender. A `done` event for repo-a should only be processed by repo-a's
/// subscriber — repo-b's subscriber sees it but skips (repo mismatch).
#[tokio::test]
async fn unblock_subscriber_only_handles_its_repo() {
    let tmp_a = temp_db("unblock-multi-a");
    let tmp_b = temp_db("unblock-multi-b");

    let store_a = Arc::new(TaskStore::open_single(&tmp_a).await.expect("open a"));
    let store_b = Arc::new(TaskStore::open_single(&tmp_b).await.expect("open b"));
    let backend_a: Arc<dyn ExternalBackend> = Arc::new(MockBackend);
    let backend_b: Arc<dyn ExternalBackend> = Arc::new(MockBackend);

    let tm_a = Arc::new(TaskManager::with_store(
        backend_a.clone(),
        store_a.clone(),
        "repo-a/proj".to_string(),
    ));
    let tm_b = Arc::new(TaskManager::with_store(
        backend_b.clone(),
        store_b.clone(),
        "repo-b/proj".to_string(),
    ));

    let (tx, _) = broadcast::channel::<TaskEvent>(16);
    let rx_a = tx.subscribe();
    let rx_b = tx.subscribe();

    unblock::spawn(rx_a, backend_a, tm_a, store_a, "repo-a/proj".to_string());
    unblock::spawn(rx_b, backend_b, tm_b, store_b, "repo-b/proj".to_string());

    // done event for repo-a: repo-a subscriber processes it, repo-b skips.
    tx.send(make_event("5", "repo-a/proj", "done")).unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    // No panic from either subscriber — test passes.
    cleanup_db(&tmp_a);
    cleanup_db(&tmp_b);
}

// ---------------------------------------------------------------------------
// review subscriber tests
//
// The review subscriber reacts to `needs_review` events. It has a dispatching
// guard that prevents concurrent double-spawn for the same task.
// ---------------------------------------------------------------------------

async fn make_review_harness(
    repo: &str,
) -> (
    broadcast::Sender<TaskEvent>,
    Arc<DashSet<String>>,
    std::path::PathBuf,
) {
    let tmp = temp_db(&format!("review-{}", repo.replace('/', "-")));
    let store = Arc::new(TaskStore::open_single(&tmp).await.expect("open store"));
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend);
    let tmux = Arc::new(TmuxManager::new());
    let semaphore = Arc::new(Semaphore::new(4));
    let task_manager = Arc::new(TaskManager::with_store(
        backend.clone(),
        store.clone(),
        repo.to_string(),
    ));
    // No available agents — router is a no-op in these tests.
    let config = RouterConfig {
        mode: "round_robin".to_string(),
        agents: vec![],
        ..RouterConfig::default()
    };
    let router = Arc::new(RwLock::new(Router::new(config)));
    let dispatching = Arc::new(DashSet::<String>::new());

    let (tx, rx) = broadcast::channel::<TaskEvent>(16);
    review::spawn(
        rx,
        backend,
        tmux,
        semaphore,
        task_manager,
        router,
        dispatching.clone(),
        store,
        repo.to_string(),
    );

    (tx, dispatching, tmp)
}

#[tokio::test]
async fn review_wrong_repo_event_ignored() {
    let (tx, _dispatching, tmp) = make_review_harness("owner/repo").await;

    tx.send(make_event("5", "other/repo", "needs_review"))
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    cleanup_db(&tmp);
}

#[tokio::test]
async fn review_wrong_status_events_ignored() {
    let (tx, _dispatching, tmp) = make_review_harness("owner/repo").await;

    // Only `needs_review` triggers the review flow.
    for status in &["routed", "done", "in_progress", "blocked", "new"] {
        tx.send(make_event("5", "owner/repo", status)).unwrap();
    }

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    cleanup_db(&tmp);
}

/// When the dispatching set already contains the task key, a concurrent
/// `needs_review` event for the same task must be skipped — the guard inside
/// the subscriber prevents double-spawn.
///
/// The dispatch key format is `"{repo}/{task_id}"`.
#[tokio::test]
async fn review_dispatching_guard_prevents_double_spawn() {
    let (tx, dispatching, tmp) = make_review_harness("owner/repo").await;

    // Pre-populate the key as if a review is already in flight.
    // This matches the format constructed in review::spawn:
    //   let dispatch_key = format!("{}/{}", repo, task_id);
    dispatching.insert("owner/repo/42".to_string());

    // The subscriber sees the key already present, logs, and continues
    // without launching a second review agent.
    tx.send(make_event("42", "owner/repo", "needs_review"))
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // Dispatching set still contains the key (subscriber did not remove it
    // since it never claimed ownership via DispatchGuard).
    assert!(
        dispatching.contains("owner/repo/42"),
        "dispatching guard should have left the pre-existing key in place"
    );
    cleanup_db(&tmp);
}

/// When a `needs_review` event arrives but the task does not exist in the
/// store, the subscriber should log and continue without panicking.
///
/// With an empty store and MockBackend returning [], task lookup returns None
/// and the subscriber hits the "task not found" early-exit path.
#[tokio::test]
async fn review_task_not_in_store_skipped_gracefully() {
    let (tx, _dispatching, tmp) = make_review_harness("owner/repo").await;

    // Task "99" is not in the store — subscriber should log and continue.
    tx.send(make_event("99", "owner/repo", "needs_review"))
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    cleanup_db(&tmp);
}

#[tokio::test]
async fn review_channel_close_exits_cleanly() {
    let (tx, _dispatching, tmp) = make_review_harness("test/repo").await;
    drop(tx);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    cleanup_db(&tmp);
}

/// Two review subscribers for different repos share the same broadcast sender.
/// A `needs_review` event for repo-a only triggers repo-a's subscriber.
#[tokio::test]
async fn review_subscriber_only_handles_its_repo() {
    let tmp_a = temp_db("review-multi-a");
    let tmp_b = temp_db("review-multi-b");

    let store_a = Arc::new(TaskStore::open_single(&tmp_a).await.expect("open a"));
    let store_b = Arc::new(TaskStore::open_single(&tmp_b).await.expect("open b"));
    let backend_a: Arc<dyn ExternalBackend> = Arc::new(MockBackend);
    let backend_b: Arc<dyn ExternalBackend> = Arc::new(MockBackend);
    let tmux = Arc::new(TmuxManager::new());
    let semaphore = Arc::new(Semaphore::new(4));
    let config = RouterConfig {
        mode: "round_robin".to_string(),
        agents: vec![],
        ..RouterConfig::default()
    };

    let tm_a = Arc::new(TaskManager::with_store(
        backend_a.clone(),
        store_a.clone(),
        "repo-a/proj".to_string(),
    ));
    let tm_b = Arc::new(TaskManager::with_store(
        backend_b.clone(),
        store_b.clone(),
        "repo-b/proj".to_string(),
    ));
    let router_a = Arc::new(RwLock::new(Router::new(config.clone())));
    let router_b = Arc::new(RwLock::new(Router::new(config)));
    let dispatching_a = Arc::new(DashSet::<String>::new());
    let dispatching_b = Arc::new(DashSet::<String>::new());

    let (tx, _) = broadcast::channel::<TaskEvent>(16);
    let rx_a = tx.subscribe();
    let rx_b = tx.subscribe();

    review::spawn(
        rx_a,
        backend_a,
        tmux.clone(),
        semaphore.clone(),
        tm_a,
        router_a,
        dispatching_a.clone(),
        store_a,
        "repo-a/proj".to_string(),
    );
    review::spawn(
        rx_b,
        backend_b,
        tmux,
        semaphore,
        tm_b,
        router_b,
        dispatching_b.clone(),
        store_b,
        "repo-b/proj".to_string(),
    );

    // Event for repo-a: repo-a subscriber claims it, repo-b skips.
    tx.send(make_event("7", "repo-a/proj", "needs_review"))
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    // Neither subscriber panicked — test passes.
    cleanup_db(&tmp_a);
    cleanup_db(&tmp_b);
}
