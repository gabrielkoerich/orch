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

    raft.stop().await;
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

    raft.stop().await;
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
        raft.stop().await;
    }

    let store = open(dir.path()).await;
    let raft = start_single(&store, &dir.path().join("raft.db"))
        .await
        .unwrap();
    assert_eq!(store.kv_increment("failure_count:codex").await.unwrap(), 3);
    raft.stop().await;
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

    raft.stop().await;
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
        raft.stop().await;
        id
    };

    let store = open(dir.path()).await;
    let raft = start_single(&store, &dir.path().join("raft.db"))
        .await
        .unwrap();
    assert_eq!(store.increment(id, "push_failures").await.unwrap(), 2);
    raft.stop().await;
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

#[tokio::test]
async fn dependency_job_and_channel_writes_go_through_the_log_and_the_snapshot() {
    use crate::store::{DepSource, JobState, NewDependency};
    let a_dir = tempfile::tempdir().unwrap();
    let a = open(a_dir.path()).await;
    let raft = start_single(&a, &a_dir.path().join("raft.db"))
        .await
        .unwrap();
    let task = a.create(&new_task("a", "")).await.unwrap();
    let before = log_len(a_dir.path()).await;
    let dep = NewDependency {
        repo: "owner/repo".into(),
        number: 7,
        source: DepSource::Cli,
    };
    a.replace_dependencies(task, std::slice::from_ref(&dep))
        .await
        .unwrap();
    assert_eq!(
        a.block_claim_if_waiting(task).await.unwrap(),
        Some(vec!["#7".to_string()])
    );
    a.upsert_job_state(&JobState {
        repo: "owner/repo".into(),
        job_id: "nightly".into(),
        last_run: Some("2026-01-01T00:00:00Z".into()),
        last_task_status: None,
        active_task_id: None,
    })
    .await
    .unwrap();
    a.subscribe_channel("telegram", "1", "owner/repo", None)
        .await
        .unwrap();
    assert_eq!(log_len(a_dir.path()).await, before + 4);

    a.kv_set("cooldown:x", "1").await.unwrap();
    let snap = raft.raft.trigger().snapshot().await;
    assert!(snap.is_ok());
    raft.stop().await;
    let mut a_raft = RaftStore::open(&a_dir.path().join("raft.db"), a.clone())
        .await
        .unwrap();
    let snap = a_raft.build_snapshot().await.unwrap();

    let b_dir = tempfile::tempdir().unwrap();
    let b = open(b_dir.path()).await;
    b.subscribe_channel("discord", "9", "other/repo", None)
        .await
        .unwrap();
    let mut b_raft = RaftStore::open(&b_dir.path().join("raft.db"), b.clone())
        .await
        .unwrap();
    b_raft
        .install_snapshot(&snap.meta, snap.snapshot)
        .await
        .unwrap();
    assert_eq!(b.list_dependencies(task).await.unwrap().len(), 1);
    assert_eq!(b.list_job_states("owner/repo").await.unwrap().len(), 1);
    assert_eq!(
        b.list_subscribers_for_repo("owner/repo")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(b
        .list_subscribers_for_repo("other/repo")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn cli_writes_go_through_the_daemon_log() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = open(dir.path()).await;
    let raft = start_single(&daemon, &dir.path().join("raft.db"))
        .await
        .unwrap();
    let endpoint = local::serve(daemon.clone()).await.unwrap();

    let cli = open(dir.path()).await;
    cli.attach_daemon(endpoint.clone());
    let before = log_len(dir.path()).await;
    cli.kv_set("cooldown:claude", "9").await.unwrap();
    let id = cli.create(&new_task("from cli", "")).await.unwrap();
    assert_eq!(log_len(dir.path()).await, before + 2);
    assert_eq!(
        daemon.kv_get("cooldown:claude").await.unwrap().as_deref(),
        Some("9")
    );
    assert_eq!(daemon.get(id).await.unwrap().title, "from cli");

    let err = cli
        .set_fields(id, &[("no_such_column", serde_json::json!(1))])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("allowlist"), "{err:#}");
    raft.stop().await;
}

#[tokio::test]
async fn cli_writes_directly_when_no_daemon_listens() {
    let dir = tempfile::tempdir().unwrap();
    let cli = open(dir.path()).await;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    cli.attach_daemon(local::Endpoint {
        url: format!("http://127.0.0.1:{port}/write"),
        token: "t".into(),
    });
    cli.kv_set("cooldown:claude", "1").await.unwrap();
    assert_eq!(
        cli.kv_get("cooldown:claude").await.unwrap().as_deref(),
        Some("1")
    );
}

#[tokio::test]
async fn daemon_endpoint_refuses_a_wrong_token_and_a_foreign_host() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = open(dir.path()).await;
    let endpoint = local::serve(daemon.clone()).await.unwrap();
    let cmd = Command::KvSet {
        key: "cooldown:claude".into(),
        value: "1".into(),
    };

    let cli = open(dir.path()).await;
    cli.attach_daemon(local::Endpoint {
        url: endpoint.url.clone(),
        token: "wrong".into(),
    });
    let err = cli.kv_set("cooldown:claude", "1").await.unwrap_err();
    assert!(err.to_string().contains("401"), "{err:#}");
    assert_eq!(daemon.kv_get("cooldown:claude").await.unwrap(), None);

    let port = endpoint
        .url
        .split(':')
        .nth(2)
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    let res = reqwest::Client::new()
        .post(format!("http://localhost:{port}/write"))
        .bearer_auth(&endpoint.token)
        .json(&cmd)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401, "a Host other than 127.0.0.1 is refused");

    let body = std::fs::read_to_string(std::env::temp_dir().join("orch-test-state/cluster.port"))
        .unwrap_or_default();
    if body.contains(&endpoint.token) {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(std::env::temp_dir().join("orch-test-state/cluster.port"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

fn free_addr() -> String {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .to_string()
}

struct Member {
    store: TaskStore,
    node: Node,
}

async fn start_member(
    dir: &Path,
    id: NodeId,
    voters: &BTreeMap<NodeId, String>,
    secret: &str,
    elect: bool,
) -> Member {
    let store = open(dir).await;
    let node = start_node(
        &store,
        &dir.join("raft.db"),
        NodeConfig {
            name: format!("node{id}"),
            id,
            addr: voters[&id].clone(),
            secret: secret.into(),
            voters: voters.clone(),
            elect,
        },
    )
    .await
    .unwrap();
    Member { store, node }
}

async fn leader_of(members: &BTreeMap<NodeId, Member>) -> NodeId {
    for _ in 0..200 {
        for (id, m) in members {
            if m.node.raft.metrics().borrow().current_leader == Some(*id) {
                return *id;
            }
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    panic!("no leader elected");
}

async fn wait_for_task(store: &TaskStore, id: i64) -> crate::store::Task {
    for _ in 0..200 {
        if let Ok(t) = store.get(id).await {
            return t;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    panic!("task {id} never arrived");
}

fn voters(n: u64) -> BTreeMap<NodeId, String> {
    (1..=n).map(|id| (id, free_addr())).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn three_voters_fail_over_and_catch_up() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let voters = voters(3);
    let mut members = BTreeMap::new();
    for id in 1..=3 {
        let m = start_member(dirs[id as usize - 1].path(), id, &voters, "s3cret", true).await;
        members.insert(id, m);
    }

    let first = leader_of(&members).await;
    let follower = *members.keys().find(|id| **id != first).unwrap();
    let a = members[&follower]
        .store
        .create(&new_task("a", "body a"))
        .await
        .unwrap();
    for m in members.values() {
        assert_eq!(wait_for_task(&m.store, a).await.title, "a");
    }

    members.remove(&first).unwrap().node.stop().await;
    let second = leader_of(&members).await;
    assert_ne!(first, second);
    let any = members.values().next().unwrap();
    let b = any.store.create(&new_task("b", "")).await.unwrap();
    assert_eq!(b, a + 1);

    let back = start_member(
        dirs[first as usize - 1].path(),
        first,
        &voters,
        "s3cret",
        true,
    )
    .await;
    assert_eq!(wait_for_task(&back.store, b).await.title, "b");
    back.node.stop().await;
    for (_, m) in members {
        m.node.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_witness_votes_but_never_leads() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let voters = voters(3);
    let mut members = BTreeMap::new();
    for id in 1..=3 {
        let witness = id == 3;
        let m = start_member(
            dirs[id as usize - 1].path(),
            id,
            &voters,
            "s3cret",
            !witness,
        )
        .await;
        members.insert(id, m);
    }

    let first = leader_of(&members).await;
    assert_ne!(first, 3);
    members.remove(&first).unwrap().node.stop().await;
    let second = leader_of(&members).await;
    assert_ne!(second, 3, "the witness took the lead");
    assert_ne!(second, first);
    let id = members[&3]
        .store
        .create(&new_task("via witness", ""))
        .await
        .unwrap();
    assert_eq!(
        wait_for_task(&members[&second].store, id).await.title,
        "via witness"
    );
    for (_, m) in members {
        m.node.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_with_the_wrong_secret_gets_nothing() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let voters = voters(3);
    let mut members = BTreeMap::new();
    for id in 1..=3 {
        let secret = if id == 3 { "other" } else { "s3cret" };
        let m = start_member(dirs[id as usize - 1].path(), id, &voters, secret, id != 3).await;
        members.insert(id, m);
    }

    let leader = leader_of(&members).await;
    let id = members[&leader]
        .store
        .create(&new_task("a", ""))
        .await
        .unwrap();
    let other = if leader == 1 { 2 } else { 1 };
    wait_for_task(&members[&other].store, id).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(members[&3].store.get(id).await.is_err());
    for (_, m) in members {
        m.node.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_learner_joins_through_any_node_and_catches_up() {
    let dirs: Vec<_> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let voters = voters(3);
    let mut members = BTreeMap::new();
    for id in 1..=3 {
        let m = start_member(dirs[id as usize - 1].path(), id, &voters, "s3cret", true).await;
        members.insert(id, m);
    }

    let leader = leader_of(&members).await;
    let before = members[&leader]
        .store
        .create(&new_task("before", ""))
        .await
        .unwrap();

    let addr = free_addr();
    let mut all = voters.clone();
    all.insert(4, addr.clone());
    let learner_store = open(dirs[3].path()).await;
    let learner = start_node(
        &learner_store,
        &dirs[3].path().join("raft.db"),
        NodeConfig {
            name: "m4".into(),
            id: 4,
            addr: addr.clone(),
            secret: "s3cret".into(),
            voters: BTreeMap::new(),
            elect: true,
        },
    )
    .await
    .unwrap();

    let follower = *members.keys().find(|id| **id != leader).unwrap();
    let hello = network::probe(
        &voters[&follower],
        "s3cret",
        Some(&format!("node{follower}")),
    )
    .await
    .unwrap();
    assert_eq!(
        (hello.name.as_str(), hello.leader),
        (format!("node{follower}").as_str(), Some(leader))
    );
    assert!(network::probe(&voters[&follower], "wrong", None)
        .await
        .is_none());
    assert!(
        network::probe(&voters[&follower], "s3cret", Some("impostor"))
            .await
            .is_none()
    );

    network::send_join(&voters[&follower], "s3cret", &network::Join { id: 4, addr })
        .await
        .unwrap();
    assert_eq!(wait_for_task(&learner_store, before).await.title, "before");
    let after = learner_store
        .create(&new_task("from learner", ""))
        .await
        .unwrap();
    assert_eq!(
        wait_for_task(&members[&leader].store, after).await.title,
        "from learner"
    );

    learner.stop().await;
    for (_, m) in members {
        m.node.stop().await;
    }
}

#[tokio::test]
async fn probe_never_sends_the_secret_and_rejects_a_peer_without_proof() {
    use axum::http::HeaderMap;
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let record = seen.clone();
    let fake = axum::Router::new().route(
        "/cluster/hello",
        axum::routing::get(move |headers: HeaderMap| {
            let record = record.clone();
            async move {
                let names = headers.keys().map(|k| k.to_string()).collect::<Vec<_>>();
                let values = headers
                    .values()
                    .filter_map(|v| v.to_str().ok().map(str::to_string))
                    .collect::<Vec<_>>();
                record
                    .lock()
                    .unwrap()
                    .extend(names.into_iter().chain(values));
                axum::Json(network::Hello {
                    name: "impostor".into(),
                    id: 9,
                    witness: false,
                    leader: None,
                    term: 0,
                    proof: "00".into(),
                })
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(listener, fake).await.ok() });

    // The probe gives up after 1 s, which a loaded test run can exceed, so it tries a few times
    for _ in 0..5 {
        assert!(network::probe(&addr, "s3cret", None).await.is_none());
        if !seen.lock().unwrap().is_empty() {
            break;
        }
    }

    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty(), "the fake peer was probed");
    assert!(
        !seen.iter().any(|v| v.contains("s3cret")),
        "the secret reached the peer: {seen:?}"
    );
    assert!(!seen.iter().any(|v| v == "x-orch-cluster-secret"));
}

fn git_ok(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_head(dir: &Path, rev: &str) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", rev])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test]
async fn progress_commits_what_is_left_pushes_and_keeps_the_last_five() {
    let root = tempfile::tempdir().unwrap();
    let bare = root.path().join("origin.git");
    let wt = root.path().join("wt");
    git_ok(
        root.path(),
        &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    git_ok(
        root.path(),
        &["clone", bare.to_str().unwrap(), wt.to_str().unwrap()],
    );
    git_ok(&wt, &["config", "user.name", "T"]);
    git_ok(&wt, &["config", "user.email", "t@t"]);
    git_ok(&wt, &["config", "commit.gpgsign", "false"]);
    std::fs::write(wt.join("a.txt"), "a").unwrap();
    git_ok(&wt, &["add", "a.txt"]);
    git_ok(&wt, &["commit", "-m", "base"]);
    git_ok(&wt, &["push", "origin", "main"]);
    git_ok(&wt, &["checkout", "-b", "gh-issue-1"]);

    let store = TaskStore::open_memory().await.unwrap();
    let id = store.create(&new_task("t", "")).await.unwrap();
    let path = wt.display().to_string();
    store
        .set_fields(id, &[("worktree", serde_json::json!(path))])
        .await
        .unwrap();

    std::fs::write(wt.join("b.txt"), "left by the agent").unwrap();
    let report = progress::Report {
        worktree: path.clone(),
        done: "step one".into(),
        left: "step two".into(),
        state: "green".into(),
        cli_committed: false,
        cli_error: None,
    };
    let record = progress::handle(&store, "m4", report.clone())
        .await
        .unwrap();
    assert_eq!(record.committed_by, "orch");
    assert_eq!(record.pushed_by, "orch");
    assert_eq!(record.branch, "gh-issue-1");
    assert_eq!(git_head(&bare, "refs/heads/gh-issue-1"), record.commit);

    for n in 0..6 {
        progress::handle(
            &store,
            "m4",
            progress::Report {
                done: format!("step {n}"),
                ..report.clone()
            },
        )
        .await
        .unwrap();
    }

    let records = progress::list(&store, id).await.unwrap();
    assert_eq!(records.len(), 5);
    assert_eq!(records.last().unwrap().done, "step 5");
    assert_eq!(records.last().unwrap().committed_by, "none");
    assert_eq!(records.last().unwrap().pushed_by, "agent");

    let elsewhere = progress::Report {
        worktree: root.path().display().to_string(),
        ..report
    };
    assert!(progress::handle(&store, "m4", elsewhere).await.is_err());
}

#[tokio::test]
async fn a_store_leads_alone_and_one_node_of_three_leads() {
    assert!(TaskStore::open_memory().await.unwrap().is_leader());

    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path()).await;
    let node = start_single(&store, &dir.path().join("raft.db"))
        .await
        .unwrap();
    assert!(store.is_leader());
    node.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn exactly_one_of_three_nodes_leads() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let voters = voters(3);
    let mut members = BTreeMap::new();
    for id in 1..=3 {
        let m = start_member(dirs[id as usize - 1].path(), id, &voters, "s3cret", true).await;
        members.insert(id, m);
    }

    let leader = leader_of(&members).await;
    for _ in 0..40 {
        let leading: Vec<NodeId> = members
            .iter()
            .filter(|(_, m)| m.store.is_leader())
            .map(|(id, _)| *id)
            .collect();
        if leading == [leader] {
            break;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    for (id, m) in &members {
        assert_eq!(m.store.is_leader(), *id == leader, "node {id}");
    }

    for (_, m) in members {
        m.node.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_relayed_proof_is_rejected() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let voters = voters(3);
    let mut members = BTreeMap::new();
    for id in 1..=3 {
        let m = start_member(dirs[id as usize - 1].path(), id, &voters, "s3cret", true).await;
        members.insert(id, m);
    }

    leader_of(&members).await;
    let real = voters[&1].clone();
    // A host that forwards the probe to a real member and hands back its genuine answer
    let relay = axum::Router::new().route(
        "/cluster/hello",
        axum::routing::get(move |headers: axum::http::HeaderMap| {
            let real = real.clone();
            async move {
                let mut req = reqwest::Client::new().get(format!("http://{real}/cluster/hello"));
                for (k, v) in headers.iter() {
                    if k.as_str().starts_with("x-orch") {
                        req = req.header(k.as_str(), v.as_bytes());
                    }
                }
                let body = req.send().await.unwrap().text().await.unwrap();
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    body,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(listener, relay).await.ok() });

    assert!(network::probe(&voters[&1], "s3cret", Some("node1"))
        .await
        .is_some());
    assert!(network::probe(&relay_addr, "s3cret", None).await.is_none());
    for (_, m) in members {
        m.node.stop().await;
    }
}
