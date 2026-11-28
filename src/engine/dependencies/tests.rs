use super::*;
use crate::backends::PullRequestCompletion;
use crate::backends::github;
use crate::backends::github::AutomatedReviewComment;
use crate::backends::test_helpers::NoopBackend;
use crate::backends::{BlockedByIssue, ExternalTask, Status};
use crate::cooldown;
use crate::engine::router::Router;
use crate::engine::router::RouterConfig;
use crate::engine::tasks;
use crate::engine::tasks::TaskManager;
use crate::engine::tasks::store_task_to_external;
use crate::engine::tick::RoutingTickQuota;
use crate::engine::tick::tick_route_tasks;
use crate::store::WaitingOnDependency;
use crate::store::{NewTask, TaskStatus};
use async_trait::async_trait;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

#[test]
fn body_parser_reads_the_three_forms_and_skips_fences() {
    let body = "\
Depends on #12
Blocked by #12, #13
Depends on owner/repo#12
```
Depends on #99
```
See #4 in the notes
";
    let deps = parse_body_dependencies(body, "acme/app");
    assert_eq!(
        deps,
        vec![
            DepRef {
                repo: "acme/app".into(),
                number: 12
            },
            DepRef {
                repo: "acme/app".into(),
                number: 13
            },
            DepRef {
                repo: "owner/repo".into(),
                number: 12
            },
        ]
    );
}

#[derive(Clone)]
struct ScriptedReviewComment {
    login: String,
    body: String,
    created_at: String,
}

struct ScriptedBackend {
    blocked: Mutex<HashMap<i64, BlockedByFetch>>,
    comments: Mutex<Vec<(String, String)>>,
    close_state: Mutex<HashMap<i64, (String, Option<String>)>>,
    pulls: Mutex<HashMap<i64, Option<PullRequestCompletion>>>,
    review_comments: Mutex<HashMap<i64, Vec<ScriptedReviewComment>>>,
    reviewer: Mutex<Option<String>>,
    /// Counts the blocker reads that cost a GitHub call.
    reads: AtomicUsize,
    /// Counts the blocked-by reads.
    blocked_reads: AtomicUsize,
}

impl ScriptedBackend {
    fn new(blocked: HashMap<i64, BlockedByFetch>) -> Self {
        Self {
            blocked: Mutex::new(blocked),
            comments: Mutex::new(Vec::new()),
            close_state: Mutex::new(HashMap::new()),
            pulls: Mutex::new(HashMap::new()),
            review_comments: Mutex::new(HashMap::new()),
            reads: AtomicUsize::new(0),
            blocked_reads: AtomicUsize::new(0),
            reviewer: Mutex::new(None),
        }
    }
}

#[async_trait]
impl ExternalBackend for ScriptedBackend {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn create_task(
        &self,
        _title: &str,
        _body: &str,
        _labels: &[String],
    ) -> anyhow::Result<ExternalId> {
        Ok(ExternalId("1".into()))
    }

    async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask> {
        Ok(ExternalTask {
            id: id.clone(),
            title: "t".into(),
            body: String::new(),
            state: "open".into(),
            labels: vec![],
            author: "bot".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            url: String::new(),
        })
    }

    async fn list_by_status(&self, _status: Status) -> anyhow::Result<Vec<ExternalTask>> {
        Ok(vec![])
    }

    async fn post_comment(&self, id: &ExternalId, body: &str) -> anyhow::Result<()> {
        self.comments
            .lock()
            .unwrap()
            .push((id.0.clone(), body.to_string()));
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

    async fn blocked_by(&self, _repo: &str, issue_number: i64) -> anyhow::Result<BlockedByFetch> {
        self.blocked_reads.fetch_add(1, Ordering::Relaxed);
        Ok(self
            .blocked
            .lock()
            .unwrap()
            .get(&issue_number)
            .cloned()
            .unwrap_or(BlockedByFetch::Unavailable))
    }

    async fn issue_close_state(
        &self,
        _repo: &str,
        issue_number: i64,
    ) -> anyhow::Result<Option<(String, Option<String>)>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        Ok(self.close_state.lock().unwrap().get(&issue_number).cloned())
    }

    async fn pull_request_completion(
        &self,
        _repo: &str,
        pr_number: i64,
    ) -> anyhow::Result<Option<PullRequestCompletion>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let mut completion = self
            .pulls
            .lock()
            .unwrap()
            .get(&pr_number)
            .cloned()
            .unwrap_or(None);
        if let Some(ref mut done) = completion
            && !done.approved
        {
            let comments = self
                .review_comments
                .lock()
                .unwrap()
                .get(&pr_number)
                .cloned()
                .unwrap_or_default();
            let reviewer = self.reviewer.lock().unwrap().clone().unwrap_or_default();
            let mapped: Vec<AutomatedReviewComment<'_>> = comments
                .iter()
                .map(|comment| crate::backends::github::AutomatedReviewComment {
                    login: &comment.login,
                    body: &comment.body,
                    created_at: &comment.created_at,
                })
                .collect();
            done.approved = github::latest_automated_review_approves(&mapped, &reviewer);
        }
        Ok(completion)
    }
}

async fn insert(store: &TaskStore, repo: &str, number: &str, title: &str, body: &str) -> i64 {
    store
        .create(&NewTask {
            external_id: Some(number.to_string()),
            repo: repo.to_string(),
            origin: "github".to_string(),
            title: title.to_string(),
            body: body.to_string(),
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

fn native(repo: &str, number: i64, state: &str, reason: Option<&str>) -> BlockedByIssue {
    BlockedByIssue {
        github_id: number,
        repo: repo.to_string(),
        number,
        state: state.to_string(),
        state_reason: reason.map(str::to_string),
    }
}

struct TestNowGuard;

impl TestNowGuard {
    fn set(ts: i64) -> Self {
        TEST_NOW_TS.store(ts, Ordering::Relaxed);
        Self
    }
}

impl Drop for TestNowGuard {
    fn drop(&mut self) {
        TEST_NOW_TS.store(0, Ordering::Relaxed);
    }
}

#[tokio::test]
async fn ingest_then_tick_reads_each_issue_once() {
    let store = TaskStore::open_memory().await.unwrap();
    let a = insert(&store, "owner/repo", "1", "A", "").await;
    let b = insert(&store, "owner/repo", "2", "B", "").await;
    let backend = ScriptedBackend::new(HashMap::new());
    for (id, number) in [(a, 1), (b, 2)] {
        sync_issue_dependencies(&backend, &store, id, "owner/repo", Some(number), "")
            .await
            .unwrap();
    }
    reconcile_dependencies(&backend, &store, None)
        .await
        .unwrap();
    assert_eq!(backend.blocked_reads.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn ingest_then_reconcile_after_sync_interval_reads_each_issue_once() {
    let store = TaskStore::open_memory().await.unwrap();
    let a = insert(&store, "owner/repo", "1", "A", "").await;
    let b = insert(&store, "owner/repo", "2", "B", "").await;
    let t0 = 1_700_000_000;
    let _now = TestNowGuard::set(t0);
    let backend = ScriptedBackend::new(HashMap::new());
    for (id, number) in [(a, 1), (b, 2)] {
        sync_issue_dependencies(&backend, &store, id, "owner/repo", Some(number), "")
            .await
            .unwrap();
    }
    TEST_NOW_TS.store(t0 + 45, Ordering::Relaxed);
    reconcile_dependencies(&backend, &store, None)
        .await
        .unwrap();
    assert_eq!(backend.blocked_reads.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn a_transport_error_ends_the_pass() {
    let store = TaskStore::open_memory().await.unwrap();
    let mut blocked = HashMap::new();
    for number in 1..=3 {
        insert(&store, "owner/repo", &number.to_string(), "T", "").await;
        blocked.insert(number, BlockedByFetch::Unreachable("down".into()));
    }
    let backend = ScriptedBackend::new(blocked);
    reconcile_dependencies(&backend, &store, None)
        .await
        .unwrap();
    assert_eq!(backend.blocked_reads.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn native_list_replaces_rows_and_empty_native_clears_them() {
    let store = TaskStore::open_memory().await.unwrap();
    let b = insert(&store, "owner/repo", "2", "B", "Depends on #1").await;
    let mut blocked = HashMap::new();
    blocked.insert(
        2,
        BlockedByFetch::Native(vec![native("owner/repo", 1, "open", None)]),
    );
    let backend = ScriptedBackend::new(blocked);
    sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "Depends on #1")
        .await
        .unwrap();
    let deps = store.list_dependencies(b).await.unwrap();
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0].source, "github");

    backend
        .blocked
        .lock()
        .unwrap()
        .insert(2, BlockedByFetch::Native(vec![]));
    sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "")
        .await
        .unwrap();
    assert!(store.list_dependencies(b).await.unwrap().is_empty());
    apply_claim_gates(&store).await.unwrap();
    assert_eq!(store.get(b).await.unwrap().status, TaskStatus::New);
}

#[tokio::test]
async fn unavailable_keeps_rows_when_the_body_has_no_deps_and_failed_keeps_rows() {
    let store = TaskStore::open_memory().await.unwrap();
    let b = insert(&store, "owner/repo", "2", "B", "").await;
    store
        .replace_dependencies(
            b,
            &[NewDependency {
                repo: "owner/repo".into(),
                number: 1,
                source: DepSource::Cli,
            }],
        )
        .await
        .unwrap();
    let backend = ScriptedBackend::new(HashMap::new());
    sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "")
        .await
        .unwrap();
    assert_eq!(store.list_dependencies(b).await.unwrap()[0].source, "cli");

    backend
        .blocked
        .lock()
        .unwrap()
        .insert(2, BlockedByFetch::Failed("boom".into()));
    sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "Depends on #9")
        .await
        .unwrap();
    assert_eq!(
        store.list_dependencies(b).await.unwrap()[0].depends_on_number,
        1
    );

    backend
        .blocked
        .lock()
        .unwrap()
        .insert(2, BlockedByFetch::Native(vec![]));
    sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "")
        .await
        .unwrap();
    assert_eq!(store.list_dependencies(b).await.unwrap()[0].source, "cli");
}

#[tokio::test]
async fn body_lines_become_rows_in_all_three_forms() {
    let store = TaskStore::open_memory().await.unwrap();
    let b = insert(
        &store,
        "acme/app",
        "8",
        "B",
        "Blocked by #12, #13\nDepends on owner/repo#12",
    )
    .await;
    let backend = ScriptedBackend::new(HashMap::new());
    sync_issue_dependencies(
        &backend,
        &store,
        b,
        "acme/app",
        Some(8),
        "Blocked by #12, #13\nDepends on owner/repo#12",
    )
    .await
    .unwrap();
    let deps = store.list_dependencies(b).await.unwrap();
    let labels: Vec<_> = deps
        .iter()
        .map(|dep| format!("{}#{}", dep.depends_on_repo, dep.depends_on_number))
        .collect();
    assert_eq!(
        labels,
        vec![
            "acme/app#12".to_string(),
            "acme/app#13".to_string(),
            "owner/repo#12".to_string()
        ]
    );
    assert!(deps.iter().all(|dep| dep.source == "body"));
}

#[tokio::test]
async fn body_dependency_is_stored_when_the_native_list_is_empty() {
    let store = TaskStore::open_memory().await.unwrap();
    let b = insert(&store, "owner/repo", "2", "B", "Depends on #1").await;
    let mut blocked = HashMap::new();
    blocked.insert(2, BlockedByFetch::Native(vec![]));
    let backend = ScriptedBackend::new(blocked);
    sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "Depends on #1")
        .await
        .unwrap();
    let deps = store.list_dependencies(b).await.unwrap();
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0].depends_on_number, 1);
    assert_eq!(deps[0].source, "body");
    apply_claim_gates(&store).await.unwrap();
    assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);
}

#[tokio::test]
async fn a_reopened_body_dependency_holds_the_waiter_again() {
    let store = TaskStore::open_memory().await.unwrap();
    let b = insert(&store, "owner/repo", "2", "B", "Depends on #1").await;
    let mut blocked = HashMap::new();
    blocked.insert(2, BlockedByFetch::Native(vec![]));
    let backend = ScriptedBackend::new(blocked);
    backend
        .close_state
        .lock()
        .unwrap()
        .insert(1, ("closed".into(), Some("completed".into())));

    sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "Depends on #1")
        .await
        .unwrap();
    assert_eq!(store.block_claim_if_waiting(b).await.unwrap(), None);
    assert_eq!(store.get(b).await.unwrap().status, TaskStatus::New);

    backend
        .close_state
        .lock()
        .unwrap()
        .insert(1, ("open".into(), Some("reopened".into())));
    sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "Depends on #1")
        .await
        .unwrap();
    assert!(store.block_claim_if_waiting(b).await.unwrap().is_some());
    assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);
}

#[tokio::test]
async fn a_reopened_cli_dependency_holds_the_waiter_again() {
    let store = TaskStore::open_memory().await.unwrap();
    let b = insert(&store, "owner/repo", "2", "B", "").await;
    store
        .replace_dependencies(
            b,
            &[NewDependency {
                repo: "owner/repo".into(),
                number: 1,
                source: DepSource::Cli,
            }],
        )
        .await
        .unwrap();
    let backend = ScriptedBackend::new(HashMap::new());
    backend
        .close_state
        .lock()
        .unwrap()
        .insert(1, ("closed".into(), Some("completed".into())));

    sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "")
        .await
        .unwrap();
    assert_eq!(store.block_claim_if_waiting(b).await.unwrap(), None);
    assert_eq!(store.get(b).await.unwrap().status, TaskStatus::New);

    backend
        .close_state
        .lock()
        .unwrap()
        .insert(1, ("open".into(), Some("reopened".into())));
    sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "")
        .await
        .unwrap();
    assert!(store.block_claim_if_waiting(b).await.unwrap().is_some());
    assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);
}

#[tokio::test]
async fn a_native_blocker_is_not_read_again_from_a_body_line() {
    let store = TaskStore::open_memory().await.unwrap();
    let b = insert(&store, "owner/repo", "2", "B", "Depends on #1").await;
    let mut blocked = HashMap::new();
    blocked.insert(
        2,
        BlockedByFetch::Native(vec![native("owner/repo", 1, "open", None)]),
    );
    let backend = ScriptedBackend::new(blocked);
    backend
        .close_state
        .lock()
        .unwrap()
        .insert(1, ("open".into(), None));
    sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "Depends on #1")
        .await
        .unwrap();
    assert_eq!(backend.reads.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn reopening_an_issue_with_a_merged_pr_keeps_the_blocker_satisfied() {
    let store = TaskStore::open_memory().await.unwrap();
    let b = insert(&store, "owner/repo", "2", "B", "Depends on #1").await;
    store
        .save_blocker_snapshot(
            "owner/repo",
            1,
            &BlockerSnapshot::observe(
                "closed",
                Some("completed"),
                Some(&ObservedPullRequest {
                    merged: true,
                    base_is_default: true,
                    approved: false,
                    closed_unmerged: false,
                }),
            ),
        )
        .await
        .unwrap();
    let mut blocked = HashMap::new();
    blocked.insert(2, BlockedByFetch::Native(vec![]));
    let backend = ScriptedBackend::new(blocked);
    backend
        .close_state
        .lock()
        .unwrap()
        .insert(1, ("open".into(), Some("reopened".into())));
    sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "Depends on #1")
        .await
        .unwrap();
    assert_eq!(store.block_claim_if_waiting(b).await.unwrap(), None);
    let snap = store
        .blocker_snapshot("owner/repo", 1)
        .await
        .unwrap()
        .unwrap();
    assert!(snap.is_satisfied());
    assert!(snap.pr_merged);
}

#[tokio::test]
async fn native_and_body_dependencies_are_both_stored() {
    let store = TaskStore::open_memory().await.unwrap();
    let body = "Depends on #3\nBlocked by #1";
    let b = insert(&store, "owner/repo", "2", "B", body).await;
    let mut blocked = HashMap::new();
    blocked.insert(
        2,
        BlockedByFetch::Native(vec![native("owner/repo", 1, "open", None)]),
    );
    let backend = ScriptedBackend::new(blocked);
    sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), body)
        .await
        .unwrap();
    let deps = store.list_dependencies(b).await.unwrap();
    let rows: Vec<_> = deps
        .iter()
        .map(|dep| (dep.depends_on_number, dep.source.as_str()))
        .collect();
    assert_eq!(rows, vec![(1, "github"), (3, "body")]);
}

#[tokio::test]
async fn removing_the_body_line_drops_only_the_body_row() {
    let store = TaskStore::open_memory().await.unwrap();
    let b = insert(&store, "owner/repo", "2", "B", "Depends on #3").await;
    let mut blocked = HashMap::new();
    blocked.insert(
        2,
        BlockedByFetch::Native(vec![native("owner/repo", 1, "open", None)]),
    );
    let backend = ScriptedBackend::new(blocked);
    sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "Depends on #3")
        .await
        .unwrap();
    assert_eq!(store.list_dependencies(b).await.unwrap().len(), 2);

    sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "")
        .await
        .unwrap();
    let deps = store.list_dependencies(b).await.unwrap();
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0].depends_on_number, 1);
    assert_eq!(deps[0].source, "github");
}

#[tokio::test]
async fn an_issue_naming_itself_in_the_body_stores_no_row() {
    let store = TaskStore::open_memory().await.unwrap();
    let b = insert(&store, "owner/repo", "2", "B", "Depends on #2").await;
    let backend = ScriptedBackend::new(HashMap::new());
    sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "Depends on #2")
        .await
        .unwrap();
    assert!(store.list_dependencies(b).await.unwrap().is_empty());
}

#[tokio::test]
async fn cycle_comments_once_and_neither_task_is_claimable() {
    let store = TaskStore::open_memory().await.unwrap();
    let a = insert(&store, "owner/repo", "1", "A", "").await;
    let b = insert(&store, "owner/repo", "2", "B", "").await;
    let mut blocked = HashMap::new();
    blocked.insert(
        1,
        BlockedByFetch::Native(vec![native("owner/repo", 2, "open", None)]),
    );
    blocked.insert(
        2,
        BlockedByFetch::Native(vec![native("owner/repo", 1, "open", None)]),
    );
    let backend = ScriptedBackend::new(blocked);
    sync_issue_dependencies(&backend, &store, a, "owner/repo", Some(1), "")
        .await
        .unwrap();
    sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "")
        .await
        .unwrap();
    apply_claim_gates(&store).await.unwrap();
    assert_eq!(store.get(a).await.unwrap().status, TaskStatus::Waiting);
    assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);
    let first = emit_dependency_notices(&backend, &store, None)
        .await
        .unwrap();
    assert_eq!(first.len(), 2);
    assert!(
        first
            .iter()
            .all(|notice| notice.comment.contains("owner/repo#1"))
    );
    assert!(
        first
            .iter()
            .all(|notice| notice.comment.contains("owner/repo#2"))
    );
    let second = emit_dependency_notices(&backend, &store, None)
        .await
        .unwrap();
    assert!(second.is_empty(), "{second:?}");
    assert_eq!(backend.comments.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn not_planned_keeps_waiting_and_notifies_once() {
    let store = TaskStore::open_memory().await.unwrap();
    let b = insert(&store, "owner/repo", "2", "B", "").await;
    let mut blocked = HashMap::new();
    blocked.insert(
        2,
        BlockedByFetch::Native(vec![native("owner/repo", 1, "closed", Some("not_planned"))]),
    );
    let backend = ScriptedBackend::new(blocked);
    let transport = Transport::new();
    let mut rx = transport.subscribe_notifications();
    sync_issue_dependencies(&backend, &store, b, "owner/repo", Some(2), "")
        .await
        .unwrap();
    apply_claim_gates(&store).await.unwrap();
    assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);
    let notices = emit_dependency_notices(&backend, &store, Some(&transport))
        .await
        .unwrap();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].notify);
    assert!(notices[0].summary.contains("not planned"));
    let again = emit_dependency_notices(&backend, &store, Some(&transport))
        .await
        .unwrap();
    assert!(again.is_empty(), "{again:?}");
    let note = rx.try_recv().unwrap();
    assert_eq!(note.status, "failed");
    assert!(rx.try_recv().is_err());
    assert_eq!(backend.comments.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn waiting_task_is_not_routed_until_the_blocker_is_done() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let a = insert(&store, "owner/repo", "1", "A", "").await;
    let b = insert(&store, "owner/repo", "2", "B", "").await;
    store
        .replace_dependencies(
            b,
            &[NewDependency {
                repo: "owner/repo".into(),
                number: 1,
                source: DepSource::Github,
            }],
        )
        .await
        .unwrap();

    let backend: Arc<dyn ExternalBackend> = Arc::new(NoopBackend);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".into(),
    ));
    let mut tier = HashMap::new();
    tier.insert("claude".to_string(), vec!["haiku".to_string()]);
    let mut model_map = HashMap::new();
    model_map.insert("simple".into(), tier.clone());
    model_map.insert("medium".into(), tier.clone());
    model_map.insert("complex".into(), tier);
    let round_robin = RouterConfig {
        mode: "round_robin".into(),
        agents: vec!["claude".into()],
        model_map: model_map.clone(),
        ..Default::default()
    };
    let mut router = Router::new_for_test(round_robin, vec!["claude".into()]);
    let mut quota = RoutingTickQuota::new(10);
    tick_route_tasks(
        &backend,
        &task_manager,
        &mut router,
        &store,
        "owner/repo",
        &mut quota,
    )
    .await
    .unwrap();
    let waiting = store.get(b).await.unwrap();
    assert_eq!(waiting.status, TaskStatus::Waiting);
    assert!(waiting.agent.is_none());

    let llm = RouterConfig {
        mode: "llm".into(),
        max_route_attempts: 0,
        agents: vec!["claude".into()],
        model_map,
        ..Default::default()
    };
    let mut llm_router = Router::new_for_test(llm, vec!["claude".into()]);
    let ext = tasks::store_task_to_external(&store.get(b).await.unwrap());
    store.update_status(b, TaskStatus::New).await.unwrap();
    let err = llm_router
        .route(&ext, &store, "owner/repo")
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<WaitingOnDependency>().is_some());
    assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);
    assert_eq!(store.get(b).await.unwrap().route_attempts, 0);

    store.update_status(a, TaskStatus::Done).await.unwrap();
    let mut quota = RoutingTickQuota::new(10);
    tick_route_tasks(
        &backend,
        &task_manager,
        &mut router,
        &store,
        "owner/repo",
        &mut quota,
    )
    .await
    .unwrap();
    assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);

    store
        .save_blocker_snapshot(
            "owner/repo",
            1,
            &BlockerSnapshot::observe(
                "closed",
                Some("completed"),
                Some(&ObservedPullRequest {
                    merged: true,
                    base_is_default: true,
                    approved: true,
                    closed_unmerged: false,
                }),
            ),
        )
        .await
        .unwrap();
    let mut quota = RoutingTickQuota::new(10);
    tick_route_tasks(
        &backend,
        &task_manager,
        &mut router,
        &store,
        "owner/repo",
        &mut quota,
    )
    .await
    .unwrap();
    let routed = store.get(b).await.unwrap();
    assert_eq!(routed.status, TaskStatus::Routed);
    assert!(routed.agent.is_some());
}

#[tokio::test]
async fn round_robin_route_returns_waiting_on_dependency() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let b = insert(&store, "owner/repo", "2", "B", "").await;
    store
        .replace_dependencies(
            b,
            &[NewDependency {
                repo: "owner/repo".into(),
                number: 1,
                source: DepSource::Github,
            }],
        )
        .await
        .unwrap();
    let ext = store_task_to_external(&store.get(b).await.unwrap());
    let mut tier = HashMap::new();
    tier.insert("claude".to_string(), vec!["haiku".to_string()]);
    let mut model_map = HashMap::new();
    model_map.insert("medium".into(), tier);
    let config = RouterConfig {
        mode: "round_robin".into(),
        agents: vec!["claude".into()],
        model_map,
        ..Default::default()
    };
    let mut router = Router::new_for_test(config, vec!["claude".into()]);
    let err = router.route(&ext, &store, "owner/repo").await.unwrap_err();
    assert!(err.downcast_ref::<WaitingOnDependency>().is_some());
    assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);
    assert_eq!(store.get(b).await.unwrap().route_attempts, 0);
}

async fn refresh_uncached(backend: &ScriptedBackend, store: &TaskStore) {
    store
        .kv_delete("dep_blocker_read_at:owner/repo#1")
        .await
        .unwrap();
    refresh_blocker_snapshots(backend, store).await.unwrap();
}

#[tokio::test]
async fn an_open_blocker_is_read_once_per_ttl() {
    let store = TaskStore::open_memory().await.unwrap();
    insert(&store, "owner/repo", "1", "A", "").await;
    let b = insert(&store, "owner/repo", "2", "B", "").await;
    store
        .replace_dependencies(
            b,
            &[NewDependency {
                repo: "owner/repo".into(),
                number: 1,
                source: DepSource::Github,
            }],
        )
        .await
        .unwrap();
    let backend = ScriptedBackend::new(HashMap::new());
    backend
        .close_state
        .lock()
        .unwrap()
        .insert(1, ("open".into(), None));
    let reads = || backend.reads.load(Ordering::Relaxed);

    refresh_blocker_snapshots(&backend, &store).await.unwrap();
    let first = reads();
    assert!(first > 0, "the first refresh has to read the blocker");

    refresh_blocker_snapshots(&backend, &store).await.unwrap();
    refresh_blocker_snapshots(&backend, &store).await.unwrap();
    assert_eq!(
        reads(),
        first,
        "a refresh inside the TTL must not call GitHub"
    );

    let stale =
        chrono::Utc::now().timestamp() - (BLOCKER_REFRESH_TTL_SECS + BLOCKER_REFRESH_SPREAD_SECS);
    store
        .kv_set("dep_blocker_read_at:owner/repo#1", &stale.to_string())
        .await
        .unwrap();
    refresh_blocker_snapshots(&backend, &store).await.unwrap();
    assert!(
        reads() > first,
        "an expired entry has to read the blocker again"
    );
}

/// Set up `#1` as an open blocker of `#2` and read it once, so the TTL is live
async fn read_open_blocker_once(store: &TaskStore) -> Arc<ScriptedBackend> {
    let b = insert(store, "owner/repo", "2", "B", "").await;
    store
        .replace_dependencies(
            b,
            &[NewDependency {
                repo: "owner/repo".into(),
                number: 1,
                source: DepSource::Github,
            }],
        )
        .await
        .unwrap();
    let backend = Arc::new(ScriptedBackend::new(HashMap::new()));
    backend
        .close_state
        .lock()
        .unwrap()
        .insert(1, ("open".into(), None));
    refresh_blocker_snapshots(&*backend, store).await.unwrap();
    backend
}

#[tokio::test]
async fn a_blockers_task_going_done_makes_the_next_refresh_read_it() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    insert(&store, "owner/repo", "1", "A", "").await;
    let backend = read_open_blocker_once(&store).await;
    let reads = || backend.reads.load(Ordering::Relaxed);
    let first = reads();
    assert!(first > 0, "the first refresh has to read the blocker");

    let task_manager = TaskManager::with_store(
        Arc::clone(&backend) as Arc<dyn ExternalBackend>,
        Arc::clone(&store),
        "owner/repo".into(),
    );
    task_manager
        .update_task_status(&ExternalId("1".into()), Status::Done)
        .await
        .unwrap();

    refresh_blocker_snapshots(&*backend, &store).await.unwrap();
    assert!(
        reads() > first,
        "a blocker whose task is done has to be read inside the TTL"
    );
}

#[tokio::test]
async fn a_blocker_orch_does_not_manage_keeps_its_ttl() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend = read_open_blocker_once(&store).await;
    let reads = || backend.reads.load(Ordering::Relaxed);
    let first = reads();
    assert!(first > 0, "the first refresh has to read the blocker");

    // `#1` has no task row here, so finishing the dependent must not clear its entry
    let task_manager = TaskManager::with_store(
        Arc::clone(&backend) as Arc<dyn ExternalBackend>,
        Arc::clone(&store),
        "owner/repo".into(),
    );
    task_manager
        .update_task_status(&ExternalId("2".into()), Status::Done)
        .await
        .unwrap();

    refresh_blocker_snapshots(&*backend, &store).await.unwrap();
    assert_eq!(
        reads(),
        first,
        "a blocker orch does not manage is read once per TTL"
    );
}

#[test]
fn blocker_ttls_spread_within_a_bounded_range() {
    let ttls: Vec<i64> = (1..200)
        .map(|n| blocker_refresh_ttl_secs("owner/repo", n))
        .collect();
    for ttl in &ttls {
        assert!(
            (BLOCKER_REFRESH_TTL_SECS..BLOCKER_REFRESH_TTL_SECS + BLOCKER_REFRESH_SPREAD_SECS)
                .contains(ttl),
            "{ttl} is outside the TTL range"
        );
    }
    let spread: HashSet<i64> = ttls.iter().copied().collect();
    assert!(spread.len() > 50, "the TTLs have to spread the reads out");
    assert_eq!(
        blocker_refresh_ttl_secs("owner/repo", 1),
        blocker_refresh_ttl_secs("owner/repo", 1),
        "one blocker keeps the same TTL across ticks"
    );
}

#[tokio::test]
async fn a_blocker_already_known_done_costs_no_github_call() {
    let store = TaskStore::open_memory().await.unwrap();
    insert(&store, "owner/repo", "1", "A", "").await;
    let b = insert(&store, "owner/repo", "2", "B", "").await;
    store
        .replace_dependencies(
            b,
            &[NewDependency {
                repo: "owner/repo".into(),
                number: 1,
                source: DepSource::Github,
            }],
        )
        .await
        .unwrap();
    let backend = ScriptedBackend::new(HashMap::new());
    backend
        .close_state
        .lock()
        .unwrap()
        .insert(1, ("closed".into(), Some("completed".into())));

    refresh_blocker_snapshots(&backend, &store).await.unwrap();
    let first = backend.reads.load(Ordering::Relaxed);
    assert!(first > 0, "the first refresh has to read the blocker");
    assert!(store.block_claim_if_waiting(b).await.unwrap().is_none());

    refresh_blocker_snapshots(&backend, &store).await.unwrap();
    refresh_blocker_snapshots(&backend, &store).await.unwrap();
    assert_eq!(
        backend.reads.load(Ordering::Relaxed),
        first,
        "a blocker closed as completed must not be read again"
    );
}

#[tokio::test]
async fn refresh_holds_until_the_pr_is_merged_into_the_default_branch() {
    let store = TaskStore::open_memory().await.unwrap();
    let a = insert(&store, "owner/repo", "1", "A", "").await;
    let b = insert(&store, "owner/repo", "2", "B", "").await;
    store
        .set_fields(a, &[("pr_number", serde_json::json!(10))])
        .await
        .unwrap();
    store
        .replace_dependencies(
            b,
            &[NewDependency {
                repo: "owner/repo".into(),
                number: 1,
                source: DepSource::Github,
            }],
        )
        .await
        .unwrap();
    let backend = ScriptedBackend::new(HashMap::new());
    backend
        .close_state
        .lock()
        .unwrap()
        .insert(1, ("closed".into(), Some("completed".into())));

    let open_pr = PullRequestCompletion {
        merged: false,
        base_ref: "main".into(),
        default_branch: "main".into(),
        approved: false,
        closed: false,
    };
    backend.pulls.lock().unwrap().insert(10, Some(open_pr));
    refresh_blocker_snapshots(&backend, &store).await.unwrap();
    assert!(store.block_claim_if_waiting(b).await.unwrap().is_some());

    backend.pulls.lock().unwrap().insert(
        10,
        Some(PullRequestCompletion {
            merged: false,
            base_ref: "main".into(),
            default_branch: "main".into(),
            approved: true,
            closed: false,
        }),
    );
    refresh_uncached(&backend, &store).await;
    assert!(store.block_claim_if_waiting(b).await.unwrap().is_some());

    backend.pulls.lock().unwrap().insert(
        10,
        Some(PullRequestCompletion {
            merged: true,
            base_ref: "feature".into(),
            default_branch: "main".into(),
            approved: true,
            closed: true,
        }),
    );
    refresh_uncached(&backend, &store).await;
    assert!(store.block_claim_if_waiting(b).await.unwrap().is_some());

    backend.pulls.lock().unwrap().insert(
        10,
        Some(PullRequestCompletion {
            merged: false,
            base_ref: "main".into(),
            default_branch: "main".into(),
            approved: true,
            closed: true,
        }),
    );
    refresh_uncached(&backend, &store).await;
    assert!(store.block_claim_if_waiting(b).await.unwrap().is_some());

    backend.pulls.lock().unwrap().insert(
        10,
        Some(PullRequestCompletion {
            merged: true,
            base_ref: "main".into(),
            default_branch: "main".into(),
            approved: false,
            closed: true,
        }),
    );
    refresh_uncached(&backend, &store).await;
    assert_eq!(store.block_claim_if_waiting(b).await.unwrap(), None);
    assert_eq!(store.get(b).await.unwrap().status, TaskStatus::New);
}

#[tokio::test]
async fn superseded_blocker_releases_the_waiter_but_a_human_close_does_not() {
    let store = TaskStore::open_memory().await.unwrap();
    let a = insert(&store, "owner/repo", "1", "A", "").await;
    let b = insert(&store, "owner/repo", "2", "B", "").await;
    store
        .set_fields(a, &[("pr_number", serde_json::json!(10))])
        .await
        .unwrap();
    store
        .replace_dependencies(
            b,
            &[NewDependency {
                repo: "owner/repo".into(),
                number: 1,
                source: DepSource::Github,
            }],
        )
        .await
        .unwrap();
    let backend = ScriptedBackend::new(HashMap::new());
    backend
        .close_state
        .lock()
        .unwrap()
        .insert(1, ("closed".into(), Some("completed".into())));
    backend.pulls.lock().unwrap().insert(
        10,
        Some(PullRequestCompletion {
            merged: false,
            base_ref: "main".into(),
            default_branch: "main".into(),
            approved: true,
            closed: true,
        }),
    );
    // A person closed the PR unmerged, so the PR stays on the task
    refresh_blocker_snapshots(&backend, &store).await.unwrap();
    assert!(store.block_claim_if_waiting(b).await.unwrap().is_some());

    // The superseded path clears the PR from the task
    store
        .set_fields(a, &[("pr_number", serde_json::json!(null))])
        .await
        .unwrap();
    refresh_uncached(&backend, &store).await;
    assert_eq!(store.block_claim_if_waiting(b).await.unwrap(), None);
}

#[tokio::test]
async fn merged_unapproved_pr_releases_the_waiter() {
    let store = TaskStore::open_memory().await.unwrap();
    let a = insert(&store, "owner/repo", "1", "A", "").await;
    let b = insert(&store, "owner/repo", "2", "B", "").await;
    store
        .set_fields(a, &[("pr_number", serde_json::json!(10))])
        .await
        .unwrap();
    store
        .replace_dependencies(
            b,
            &[NewDependency {
                repo: "owner/repo".into(),
                number: 1,
                source: DepSource::Github,
            }],
        )
        .await
        .unwrap();
    let backend = ScriptedBackend::new(HashMap::new());
    backend
        .close_state
        .lock()
        .unwrap()
        .insert(1, ("closed".into(), Some("completed".into())));
    backend.pulls.lock().unwrap().insert(
        10,
        Some(PullRequestCompletion {
            merged: true,
            base_ref: "main".into(),
            default_branch: "main".into(),
            approved: false,
            closed: true,
        }),
    );
    refresh_blocker_snapshots(&backend, &store).await.unwrap();
    assert_eq!(store.block_claim_if_waiting(b).await.unwrap(), None);
    assert_eq!(store.get(b).await.unwrap().status, TaskStatus::New);
}

#[tokio::test]
async fn blocker_closed_before_import_releases_on_refresh() {
    let store = TaskStore::open_memory().await.unwrap();
    let waiter = insert(&store, "owner/repo", "24", "later", "").await;
    let mut blocked = HashMap::new();
    blocked.insert(
        24,
        BlockedByFetch::Native(vec![native("owner/repo", 14, "open", None)]),
    );
    let backend = ScriptedBackend::new(blocked);
    backend
        .close_state
        .lock()
        .unwrap()
        .insert(14, ("closed".into(), Some("completed".into())));
    reconcile_dependencies(&backend, &store, None)
        .await
        .unwrap();
    assert_eq!(store.get(waiter).await.unwrap().status, TaskStatus::New);
    let snap = store
        .blocker_snapshot("owner/repo", 14)
        .await
        .unwrap()
        .unwrap();
    assert!(snap.is_satisfied());
    assert_eq!(snap.issue_state, "closed");
}

#[test]
fn depends_on_token_accepts_a_bare_number_and_a_cross_repo_ref() {
    assert_eq!(
        parse_depends_on_token("12", "acme/app"),
        Some(DepRef {
            repo: "acme/app".into(),
            number: 12
        })
    );
    assert_eq!(
        parse_depends_on_token("owner/repo#12", "acme/app"),
        Some(DepRef {
            repo: "owner/repo".into(),
            number: 12
        })
    );
}

#[tokio::test]
async fn two_route_calls_cannot_claim_a_waiting_task() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let b = insert(&store, "owner/repo", "2", "B", "").await;
    store
        .replace_dependencies(
            b,
            &[NewDependency {
                repo: "owner/repo".into(),
                number: 1,
                source: DepSource::Github,
            }],
        )
        .await
        .unwrap();
    let ext = store_task_to_external(&store.get(b).await.unwrap());
    let mut tier = HashMap::new();
    tier.insert("claude".to_string(), vec!["haiku".to_string()]);
    let mut model_map = HashMap::new();
    model_map.insert("medium".into(), tier);
    let config = RouterConfig {
        mode: "round_robin".into(),
        agents: vec!["claude".into()],
        model_map,
        ..Default::default()
    };
    let left_store = Arc::clone(&store);
    let right_store = Arc::clone(&store);
    let left_task = ext.clone();
    let right_task = ext;
    let right_config = config.clone();
    let left = tokio::spawn(async move {
        let mut router = Router::new_for_test(config, vec!["claude".into()]);
        router.route(&left_task, &left_store, "owner/repo").await
    });
    let right = tokio::spawn(async move {
        let mut router = Router::new_for_test(right_config, vec!["claude".into()]);
        router.route(&right_task, &right_store, "owner/repo").await
    });
    assert!(left.await.unwrap().is_err());
    assert!(right.await.unwrap().is_err());
    let task = store.get(b).await.unwrap();
    assert_eq!(task.status, TaskStatus::Waiting);
    assert!(task.agent.is_none());
}

#[tokio::test]
async fn refuse_if_waiting_blocks_manual_claim() {
    let store = TaskStore::open_memory().await.unwrap();
    let b = insert(&store, "owner/repo", "2", "B", "").await;
    store
        .replace_dependencies(
            b,
            &[NewDependency {
                repo: "owner/repo".into(),
                number: 1,
                source: DepSource::Cli,
            }],
        )
        .await
        .unwrap();
    let err = refuse_if_waiting(&store, b).await.unwrap_err();
    assert!(err.to_string().contains("#1"));
    assert_eq!(store.get(b).await.unwrap().status, TaskStatus::Waiting);
}
