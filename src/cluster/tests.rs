use super::*;
use openraft::{RaftSnapshotBuilder, RaftStorage};

async fn open(dir: &Path) -> TaskStore {
    TaskStore::open_single(&dir.join("orch.db")).await.unwrap()
}

async fn log_len(dir: &Path) -> i64 {
    let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", dir.join("raft.db").display()))
        .await
        .unwrap();
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM log")
        .fetch_one(&pool)
        .await
        .unwrap();
    n
}

#[tokio::test]
async fn single_node_applies_kv_writes_through_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path()).await;
    let raft = start_single(&store, &dir.path().join("raft.db"))
        .await
        .unwrap();

    let before = log_len(dir.path()).await;
    store.kv_set("cooldown:claude", "123").await.unwrap();
    assert_eq!(
        store.kv_get("cooldown:claude").await.unwrap().as_deref(),
        Some("123")
    );
    assert_eq!(store.kv_increment("failure_count:claude").await.unwrap(), 1);
    assert_eq!(store.kv_increment("failure_count:claude").await.unwrap(), 2);
    assert_eq!(
        store.kv_insert_if_absent("approved:1", "a").await.unwrap(),
        "a"
    );
    assert_eq!(
        store.kv_insert_if_absent("approved:1", "b").await.unwrap(),
        "a"
    );
    store.kv_delete("cooldown:claude").await.unwrap();
    assert_eq!(store.kv_get("cooldown:claude").await.unwrap(), None);
    assert_eq!(log_len(dir.path()).await, before + 6);

    raft.shutdown().await.unwrap();
}

#[tokio::test]
async fn local_keys_skip_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path()).await;
    let raft = start_single(&store, &dir.path().join("raft.db"))
        .await
        .unwrap();

    let before = log_len(dir.path()).await;
    store.kv_set("metrics:ticks", "1").await.unwrap();
    assert_eq!(store.kv_increment("metrics:ticks").await.unwrap(), 2);
    assert_eq!(log_len(dir.path()).await, before);

    raft.shutdown().await.unwrap();
}

#[tokio::test]
async fn restart_does_not_apply_an_entry_twice() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = open(dir.path()).await;
        let raft = start_single(&store, &dir.path().join("raft.db"))
            .await
            .unwrap();
        store.kv_increment("failure_count:codex").await.unwrap();
        store.kv_increment("failure_count:codex").await.unwrap();
        raft.shutdown().await.unwrap();
    }

    let store = open(dir.path()).await;
    let raft = start_single(&store, &dir.path().join("raft.db"))
        .await
        .unwrap();
    assert_eq!(store.kv_increment("failure_count:codex").await.unwrap(), 3);
    raft.shutdown().await.unwrap();
}

#[tokio::test]
async fn snapshot_moves_replicated_rows_and_keeps_local_ones() {
    let a_dir = tempfile::tempdir().unwrap();
    let a = open(a_dir.path()).await;
    a.kv_set("cooldown:claude", "1").await.unwrap();
    a.kv_set("metrics:a", "only on a").await.unwrap();
    let mut a_raft = RaftStore::open(&a_dir.path().join("raft.db"), a.clone())
        .await
        .unwrap();
    let snap = a_raft.build_snapshot().await.unwrap();

    let b_dir = tempfile::tempdir().unwrap();
    let b = open(b_dir.path()).await;
    b.kv_set("cooldown:stale", "x").await.unwrap();
    b.kv_set("metrics:b", "only on b").await.unwrap();
    let mut b_raft = RaftStore::open(&b_dir.path().join("raft.db"), b.clone())
        .await
        .unwrap();
    b_raft
        .install_snapshot(&snap.meta, snap.snapshot)
        .await
        .unwrap();

    assert_eq!(
        b.kv_get("cooldown:claude").await.unwrap().as_deref(),
        Some("1")
    );
    assert_eq!(b.kv_get("cooldown:stale").await.unwrap(), None);
    assert_eq!(
        b.kv_get("metrics:b").await.unwrap().as_deref(),
        Some("only on b")
    );
    assert_eq!(b.kv_get("metrics:a").await.unwrap(), None);
}

#[tokio::test]
async fn apply_stamps_the_proposer_time() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path()).await;
    let mut conn = store.pool().acquire().await.unwrap();
    Command::KvSet {
        key: "cooldown:claude".into(),
        value: "1".into(),
    }
    .apply(&mut conn, "2026-01-02T03:04:05Z")
    .await
    .unwrap();
    let (at,): (String,) = sqlx::query_as("SELECT updated_at FROM kv WHERE key = ?")
        .bind("cooldown:claude")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(at, "2026-01-02T03:04:05Z");
}

#[tokio::test]
async fn store_without_raft_applies_directly() {
    let store = TaskStore::open_memory().await.unwrap();
    store.kv_set("cooldown:claude", "1").await.unwrap();
    assert_eq!(store.kv_increment("failure_count:claude").await.unwrap(), 1);
    assert_eq!(
        store.kv_get("cooldown:claude").await.unwrap().as_deref(),
        Some("1")
    );
}

fn new_task(title: &str, body: &str) -> crate::store::NewTask {
    crate::store::NewTask {
        repo: "owner/repo".into(),
        origin: "internal".into(),
        title: title.into(),
        body: body.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn task_writes_go_through_the_log() {
    use crate::store::TaskStatus;
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path()).await;
    let raft = start_single(&store, &dir.path().join("raft.db"))
        .await
        .unwrap();

    let a = store.create(&new_task("a", "")).await.unwrap();
    let b = store
        .create_internal("owner/repo", "b", "", "manual", "", None)
        .await
        .unwrap();
    assert_eq!((a, b), (1, 2));
    assert_eq!(
        store.get(b).await.unwrap().external_id.as_deref(),
        Some("internal:2")
    );

    assert!(store
        .update_status_if(a, TaskStatus::Routed, TaskStatus::New)
        .await
        .unwrap());
    assert!(!store
        .update_status_if(a, TaskStatus::InProgress, TaskStatus::New)
        .await
        .unwrap());
    assert_eq!(store.get(a).await.unwrap().status, TaskStatus::Routed);
    assert_eq!(store.increment(a, "attempts").await.unwrap(), 1);

    let before = log_len(dir.path()).await;
    store
        .set_fields(a, &[("worktree", serde_json::json!("/here"))])
        .await
        .unwrap();
    assert_eq!(
        log_len(dir.path()).await,
        before,
        "a local column skips the log"
    );
    store
        .set_fields(a, &[("branch", serde_json::json!("gh-issue-1"))])
        .await
        .unwrap();
    assert_eq!(log_len(dir.path()).await, before + 1);
    let task = store.get(a).await.unwrap();
    assert_eq!(
        (task.worktree.as_str(), task.branch.as_str()),
        ("/here", "gh-issue-1")
    );

    raft.shutdown().await.unwrap();
}

#[tokio::test]
async fn restart_does_not_increment_a_task_counter_twice() {
    let dir = tempfile::tempdir().unwrap();
    let id = {
        let store = open(dir.path()).await;
        let raft = start_single(&store, &dir.path().join("raft.db"))
            .await
            .unwrap();
        let id = store.create(&new_task("a", "")).await.unwrap();
        store.increment(id, "push_failures").await.unwrap();
        raft.shutdown().await.unwrap();
        id
    };

    let store = open(dir.path()).await;
    let raft = start_single(&store, &dir.path().join("raft.db"))
        .await
        .unwrap();
    assert_eq!(store.increment(id, "push_failures").await.unwrap(), 2);
    raft.shutdown().await.unwrap();
}

#[tokio::test]
async fn snapshot_replaces_tasks_and_keeps_local_columns() {
    use crate::store::TaskStatus;
    let a_dir = tempfile::tempdir().unwrap();
    let a = open(a_dir.path()).await;
    let active = a.create(&new_task("active", "active body")).await.unwrap();
    let done = a.create(&new_task("done", "a body")).await.unwrap();
    a.update_status(done, TaskStatus::Done).await.unwrap();
    let mut a_raft = RaftStore::open(&a_dir.path().join("raft.db"), a.clone())
        .await
        .unwrap();
    let snap = a_raft.build_snapshot().await.unwrap();

    let b_dir = tempfile::tempdir().unwrap();
    let b = open(b_dir.path()).await;
    let b_active = b.create(&new_task("b title", "b body")).await.unwrap();
    b.set_fields(b_active, &[("worktree", serde_json::json!("/b/wt"))])
        .await
        .unwrap();
    let b_done = b.create(&new_task("done", "b copy")).await.unwrap();
    let stale = b.create(&new_task("stale", "")).await.unwrap();
    b.append_activity(stale, "note", None, None, None, None, None)
        .await
        .unwrap();
    assert_eq!((b_active, b_done), (active, done));

    let mut b_raft = RaftStore::open(&b_dir.path().join("raft.db"), b.clone())
        .await
        .unwrap();
    b_raft
        .install_snapshot(&snap.meta, snap.snapshot)
        .await
        .unwrap();

    let t = b.get(active).await.unwrap();
    assert_eq!(
        (t.title.as_str(), t.body.as_str()),
        ("active", "active body")
    );
    assert_eq!(
        t.worktree, "/b/wt",
        "a local column keeps this node's value"
    );
    let t = b.get(done).await.unwrap();
    assert_eq!(t.status, TaskStatus::Done);
    assert_eq!(t.body, "b copy", "a tombstone keeps this node's body");
    assert!(
        b.get(stale).await.is_err(),
        "a task missing from the snapshot goes"
    );
    assert!(b.get_activity(stale, None).await.unwrap().is_empty());
}

#[tokio::test]
async fn snapshot_inserts_a_done_task_as_a_tombstone() {
    use crate::store::TaskStatus;
    let a_dir = tempfile::tempdir().unwrap();
    let a = open(a_dir.path()).await;
    let done = a.create(&new_task("done", "a body")).await.unwrap();
    a.update_status(done, TaskStatus::Done).await.unwrap();
    let mut a_raft = RaftStore::open(&a_dir.path().join("raft.db"), a.clone())
        .await
        .unwrap();
    let snap = a_raft.build_snapshot().await.unwrap();

    let b_dir = tempfile::tempdir().unwrap();
    let b = open(b_dir.path()).await;
    let mut b_raft = RaftStore::open(&b_dir.path().join("raft.db"), b.clone())
        .await
        .unwrap();
    b_raft
        .install_snapshot(&snap.meta, snap.snapshot)
        .await
        .unwrap();
    let t = b.get(done).await.unwrap();
    assert_eq!(
        (t.title.as_str(), t.body.as_str(), t.status),
        ("done", "", TaskStatus::Done)
    );
    assert_eq!(b.create(&new_task("next", "")).await.unwrap(), done + 1);
}
