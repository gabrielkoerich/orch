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
