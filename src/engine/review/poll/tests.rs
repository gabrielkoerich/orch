use super::*;
use crate::backends::{ExternalId, ExternalTask, Mention};
use crate::engine::EngineConfig;
use crate::engine::git::cleanup;
use crate::engine::router;
use crate::engine::tasks;
use crate::github::http::PrReviewBatchData;
use crate::github::types::{GitHubComment, GitHubReview, GitHubUser};
use crate::store;
use crate::store::{NewTask, TaskStatus, TaskStore};
use async_trait::async_trait;
use dashmap::{DashMap, DashSet};
use std::collections::HashMap;
use std::io;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tokio::runtime::Builder;
use tracing::Level;
use tracing::subscriber;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriter;

// ─── Mock GhReviewClient ─────────────────────────────────────────────────

#[derive(Default)]
struct MockGh {
    /// Responses for get_pr_number keyed by branch.
    pr_numbers: HashMap<String, Option<u64>>,
    /// Responses for is_pr_merged keyed by branch.
    merged: HashMap<String, bool>,
    /// Response for batch_fetch_pr_review_data.
    batch_data: HashMap<u64, PrReviewBatchData>,
    /// Whether batch_fetch_pr_review_data should return an error.
    batch_error: bool,
    /// Responses for is_collaborator keyed by username.
    collaborators: HashMap<String, bool>,
}

#[async_trait]
impl GhReviewClient for MockGh {
    async fn get_pr_number(&self, _repo: &str, branch: &str) -> anyhow::Result<Option<u64>> {
        Ok(self.pr_numbers.get(branch).copied().flatten())
    }
    async fn is_pr_merged(&self, _repo: &str, branch: &str) -> anyhow::Result<bool> {
        Ok(self.merged.get(branch).copied().unwrap_or(false))
    }
    async fn batch_fetch_pr_review_data(
        &self,
        _repo: &str,
        _pr_numbers: &[u64],
    ) -> anyhow::Result<HashMap<u64, PrReviewBatchData>> {
        if self.batch_error {
            anyhow::bail!("simulated batch GraphQL error");
        }
        Ok(self.batch_data.clone())
    }
    async fn is_collaborator(&self, _repo: &str, username: &str) -> anyhow::Result<bool> {
        Ok(self.collaborators.get(username).copied().unwrap_or(false))
    }
}

// ─── Mock ExternalBackend ────────────────────────────────────────────────

struct MockBackend;

#[async_trait]
impl ExternalBackend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }
    async fn create_task(&self, _t: &str, _b: &str, _l: &[String]) -> anyhow::Result<ExternalId> {
        Ok(ExternalId("1".to_string()))
    }
    async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask> {
        Ok(make_ext_task(&id.0))
    }
    async fn list_by_status(&self, _s: Status) -> anyhow::Result<Vec<ExternalTask>> {
        Ok(vec![])
    }
    async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
        Ok(vec![])
    }
    async fn post_comment(&self, _id: &ExternalId, _b: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn set_labels(&self, _id: &ExternalId, _l: &[String]) -> anyhow::Result<()> {
        Ok(())
    }
    async fn remove_label(&self, _id: &ExternalId, _l: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn get_sub_issues(&self, _id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
        Ok(vec![])
    }
    async fn create_sub_task(
        &self,
        _p: &ExternalId,
        _t: &str,
        _b: &str,
        _l: &[String],
    ) -> anyhow::Result<ExternalId> {
        Ok(ExternalId("child".to_string()))
    }
    async fn ensure_status_label(&self, _l: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn has_open_issue_with_title(&self, _t: &str, _l: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn health_check(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn is_pr_merged(&self, _b: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn get_authenticated_user(&self) -> anyhow::Result<Option<String>> {
        Ok(Some("bot".to_string()))
    }
    async fn get_mentions(&self, _s: &str) -> anyhow::Result<Vec<Mention>> {
        Ok(vec![])
    }
    async fn update_status(&self, _id: &ExternalId, _s: Status) -> anyhow::Result<()> {
        Ok(())
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────

fn make_ext_task(id: &str) -> ExternalTask {
    ExternalTask {
        id: ExternalId(id.to_string()),
        title: "test task".to_string(),
        body: String::new(),
        state: "open".to_string(),
        labels: vec![],
        author: "user".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: String::new(),
    }
}

fn make_comment(id: u64, login: &str, body: &str, created_at: &str) -> GitHubComment {
    GitHubComment {
        id,
        body: body.to_string(),
        user: GitHubUser {
            login: login.to_string(),
        },
        created_at: created_at.to_string(),
        updated_at: None,
        html_url: None,
        issue_url: None,
        author_association: None,
    }
}

fn make_review(
    id: u64,
    login: &str,
    state: &str,
    submitted_at: &str,
    body: Option<&str>,
) -> GitHubReview {
    GitHubReview {
        id,
        user: GitHubUser {
            login: login.to_string(),
        },
        body: body.map(str::to_string),
        state: state.to_string(),
        html_url: None,
        submitted_at: submitted_at.to_string(),
        commit_id: None,
    }
}

async fn make_store() -> Arc<TaskStore> {
    Arc::new(TaskStore::open_memory().await.unwrap())
}

fn default_config() -> EngineConfig {
    EngineConfig::default()
}

struct CaptureWriter(Arc<Mutex<Vec<u8>>>);
impl Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter(Arc::clone(&self.0))
    }
}

fn with_captured_warn_logs<F>(f: F) -> String
where
    F: FnOnce(),
{
    let output: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(Level::WARN.into()))
        .with_writer(CaptureWriter(Arc::clone(&output)))
        .with_ansi(false)
        .finish();
    let guard = subscriber::set_default(subscriber);
    f();
    drop(guard);

    String::from_utf8_lossy(&output.lock().unwrap()).to_string()
}

#[derive(Clone)]
struct MockBatchFailStore {
    delete_error: Option<String>,
    set_error: Option<String>,
    set_calls: Arc<Mutex<Vec<(String, String)>>>,
}

#[async_trait]
impl ReviewBatchFailCounterStore for MockBatchFailStore {
    async fn kv_delete(&self, _key: &str) -> anyhow::Result<()> {
        if let Some(msg) = &self.delete_error {
            anyhow::bail!("{msg}");
        }
        Ok(())
    }

    async fn kv_set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.set_calls
            .lock()
            .unwrap()
            .push((key.to_string(), value.to_string()));
        if let Some(msg) = &self.set_error {
            anyhow::bail!("{msg}");
        }
        Ok(())
    }
}

// ─── automated_review_from_comments ──────────────────────────────────────

#[test]
fn arc_empty_input_returns_none() {
    let result = automated_review_from_comments(&[], &HashMap::new(), 1);
    assert_eq!(result, None);
}

#[test]
fn arc_non_automated_comment_ignored() {
    let comments = vec![make_comment(
        1,
        "alice",
        "Great work!",
        "2026-01-01T00:00:00Z",
    )];
    let cache = HashMap::from([("alice".to_string(), Some(true))]);
    assert_eq!(automated_review_from_comments(&comments, &cache, 1), None);
}

#[test]
fn arc_collaborator_approve_returns_approve() {
    let body = "## Automated Review \u{2014} Approve\n\nLooks good.";
    let comments = vec![make_comment(1, "bot", body, "2026-01-01T00:00:00Z")];
    let cache = HashMap::from([("bot".to_string(), Some(true))]);
    assert_eq!(
        automated_review_from_comments(&comments, &cache, 1),
        Some("approve".to_string())
    );
}

#[test]
fn arc_collaborator_changes_requested_returns_changes_requested() {
    let body = "## Automated Review \u{2014} Changes Requested\n\nFix the bug.";
    let comments = vec![make_comment(1, "bot", body, "2026-01-01T00:00:00Z")];
    let cache = HashMap::from([("bot".to_string(), Some(true))]);
    assert_eq!(
        automated_review_from_comments(&comments, &cache, 1),
        Some("changes_requested".to_string())
    );
}

#[test]
fn arc_non_collaborator_comment_ignored() {
    let body = "## Automated Review \u{2014} Approve\n\nLooks good.";
    let comments = vec![make_comment(1, "outsider", body, "2026-01-01T00:00:00Z")];
    let cache = HashMap::from([("outsider".to_string(), Some(false))]);
    assert_eq!(automated_review_from_comments(&comments, &cache, 1), None);
}

#[test]
fn arc_unknown_user_not_in_cache_is_ignored() {
    // User not present in cache at all — treated as non-collaborator.
    let body = "## Automated Review \u{2014} Approve\n\nLooks good.";
    let comments = vec![make_comment(1, "unknown", body, "2026-01-01T00:00:00Z")];
    let cache: HashMap<String, Option<bool>> = HashMap::new();
    assert_eq!(automated_review_from_comments(&comments, &cache, 1), None);
}

#[test]
fn arc_collab_check_failed_none_in_cache_skipped() {
    // Cache has the key but value is None — collaborator check failed.
    let body = "## Automated Review \u{2014} Approve\n\nLooks good.";
    let comments = vec![make_comment(1, "bot", body, "2026-01-01T00:00:00Z")];
    let cache: HashMap<String, Option<bool>> = HashMap::from([("bot".to_string(), None)]);
    assert_eq!(automated_review_from_comments(&comments, &cache, 1), None);
}

#[test]
fn arc_newest_comment_wins() {
    let body_approve = "## Automated Review \u{2014} Approve\n\nLooks good.";
    let body_changes = "## Automated Review \u{2014} Changes Requested\n\nFix the bug.";
    let comments = vec![
        make_comment(1, "bot", body_approve, "2026-01-01T00:00:00Z"),
        make_comment(2, "bot", body_changes, "2026-01-02T00:00:00Z"),
    ];
    let cache = HashMap::from([("bot".to_string(), Some(true))]);
    // Newer (changes_requested) wins.
    assert_eq!(
        automated_review_from_comments(&comments, &cache, 1),
        Some("changes_requested".to_string())
    );
}

#[test]
fn arc_automated_review_header_without_known_verdict_returns_none() {
    // Body starts with "## Automated Review" but first line doesn't match either verdict.
    let body = "## Automated Review — Unknown\n\nSome text.";
    let comments = vec![make_comment(1, "bot", body, "2026-01-01T00:00:00Z")];
    let cache = HashMap::from([("bot".to_string(), Some(true))]);
    assert_eq!(automated_review_from_comments(&comments, &cache, 1), None);
}

// ─── review_open_prs: empty task list ────────────────────────────────────

#[tokio::test]
async fn review_open_prs_empty_tasks_returns_ok_without_info_log() {
    let store = make_store().await;
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend);
    let tm = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let in_flight = Arc::new(DashSet::new());
    let gh = MockGh::default();

    let result = Box::pin(review_open_prs(
        &backend,
        "owner/repo",
        &default_config(),
        &tm,
        &store,
        &dispatching,
        &in_flight,
        &[],
        &gh,
        &router::RouterConfig::default(),
    ))
    .await;

    assert!(result.is_ok());
}

// ─── review_open_prs: dispatching set guard ───────────────────────────────

#[tokio::test]
async fn review_open_prs_dispatching_guard_skips_task() {
    let store = make_store().await;
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend);
    let tm = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));

    // Create a task in-store so we can build a snapshot.
    let task_store_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "task1".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let _ = store::store_set_by_id(
        Some(&store),
        task_store_id,
        &[("branch", serde_json::json!("my-branch"))],
    )
    .await;
    store
        .update_status(task_store_id, TaskStatus::InReview)
        .await
        .unwrap();
    let stored = store.get(task_store_id).await.unwrap();
    let snapshot = cleanup::ReviewTaskSnapshot {
        external: tasks::store_task_to_external(&stored),
        stored,
    };

    // Lock the task in the dispatching set.
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    dispatching.insert(
        format!("owner/repo/{}", snapshot.external.id.0),
        snapshot.external.id.0.clone(),
    );

    // Mock returns pr_number = Some(42) so the task would normally proceed.
    let mut gh = MockGh::default();
    gh.pr_numbers.insert("my-branch".to_string(), Some(42));
    // If the guard is broken, batch_fetch would be called (we don't set batch_data, so it
    // returns empty — which would cause the task to silently skip, not panic).
    // The important invariant is that the store status remains InReview.

    let in_flight = Arc::new(DashSet::new());
    let result = Box::pin(review_open_prs(
        &backend,
        "owner/repo",
        &default_config(),
        &tm,
        &store,
        &dispatching,
        &in_flight,
        &[snapshot],
        &gh,
        &router::RouterConfig::default(),
    ))
    .await;

    assert!(result.is_ok());
    // Task status must remain in_review (guard prevented any state change).
    let after = store.get(task_store_id).await.unwrap();
    assert_eq!(after.status, TaskStatus::InReview);
}

// ─── review_open_prs: Phase 3 — no open PR, PR merged ────────────────────

#[tokio::test]
async fn review_open_prs_no_pr_merged_marks_done() {
    let store = make_store().await;
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend);
    let tm = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "task1".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let _ = store::store_set_by_id(
        Some(&store),
        task_id,
        &[("branch", serde_json::json!("feat-branch"))],
    )
    .await;
    store
        .update_status(task_id, TaskStatus::InReview)
        .await
        .unwrap();
    let stored = store.get(task_id).await.unwrap();
    let snapshot = cleanup::ReviewTaskSnapshot {
        external: tasks::store_task_to_external(&stored),
        stored,
    };

    // No PR number stored, and no result from get_pr_number → pr_number stays None.
    // is_pr_merged returns true.
    let mut gh = MockGh::default();
    gh.merged.insert("feat-branch".to_string(), true);

    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let in_flight = Arc::new(DashSet::new());
    Box::pin(review_open_prs(
        &backend,
        "owner/repo",
        &default_config(),
        &tm,
        &store,
        &dispatching,
        &in_flight,
        &[snapshot],
        &gh,
        &router::RouterConfig::default(),
    ))
    .await
    .unwrap();

    let after = store.get(task_id).await.unwrap();
    assert_eq!(after.status, TaskStatus::Done);
}

// ─── review_open_prs: Phase 3 — no PR, not merged, reroute ──────────────

#[tokio::test]
async fn review_open_prs_no_pr_not_merged_reroutes() {
    let store = make_store().await;
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend);
    let tm = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "task2".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let _ = store::store_set_by_id(
        Some(&store),
        task_id,
        &[("branch", serde_json::json!("feat-branch"))],
    )
    .await;
    store
        .update_status(task_id, TaskStatus::InReview)
        .await
        .unwrap();
    let stored = store.get(task_id).await.unwrap();
    let snapshot = cleanup::ReviewTaskSnapshot {
        external: tasks::store_task_to_external(&stored),
        stored,
    };

    // No PR, not merged.
    let gh = MockGh::default(); // merged defaults to false, pr_numbers empty

    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let in_flight = Arc::new(DashSet::new());
    Box::pin(review_open_prs(
        &backend,
        "owner/repo",
        &default_config(),
        &tm,
        &store,
        &dispatching,
        &in_flight,
        &[snapshot],
        &gh,
        &router::RouterConfig::default(),
    ))
    .await
    .unwrap();

    let after = store.get(task_id).await.unwrap();
    assert_eq!(after.status, TaskStatus::Routed);
    assert_eq!(after.no_code_reroutes, 1);
}

// ─── review_open_prs: Phase 3 — approved PR closed without merge ──────────

#[tokio::test]
async fn review_open_prs_blocks_an_approved_task_whose_pr_closed_unmerged() {
    let store = make_store().await;
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend);
    let tm = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "approved".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let _ = store::store_set_by_id(
        Some(&store),
        task_id,
        &[("branch", serde_json::json!("approved-branch"))],
    )
    .await;
    store
        .update_status(task_id, TaskStatus::Approved)
        .await
        .unwrap();
    let stored = store.get(task_id).await.unwrap();
    let snapshot = cleanup::ReviewTaskSnapshot {
        external: tasks::store_task_to_external(&stored),
        stored,
    };

    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let in_flight = Arc::new(DashSet::new());
    Box::pin(review_open_prs(
        &backend,
        "owner/repo",
        &default_config(),
        &tm,
        &store,
        &dispatching,
        &in_flight,
        &[snapshot],
        &MockGh::default(),
        &router::RouterConfig::default(),
    ))
    .await
    .unwrap();

    let after = store.get(task_id).await.unwrap();
    assert_eq!(after.status, TaskStatus::Blocked);
    assert_eq!(
        after.block_reason.as_deref(),
        Some("approved PR was closed without merge")
    );
    assert_eq!(
        after.no_code_reroutes, 0,
        "an approved task is not rerouted"
    );
}

// ─── review_open_prs: Phase 3 — no PR, max reroutes → blocked ────────────

#[tokio::test]
async fn review_open_prs_no_pr_max_reroutes_blocks_task() {
    let store = make_store().await;
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend);
    let tm = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "task3".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let _ = store::store_set_by_id(
        Some(&store),
        task_id,
        &[("branch", serde_json::json!("feat-branch"))],
    )
    .await;
    store
        .update_status(task_id, TaskStatus::InReview)
        .await
        .unwrap();

    // Pre-set no_code_reroutes to a high value that exceeds any configured max after increment.
    // The default max is 3; use 99 to be robust against any real config on the test machine.
    let _ = store::store_set_by_id(
        Some(&store),
        task_id,
        &[("no_code_reroutes", serde_json::json!(99i64))],
    )
    .await;

    let stored = store.get(task_id).await.unwrap();
    let snapshot = cleanup::ReviewTaskSnapshot {
        external: tasks::store_task_to_external(&stored),
        stored,
    };

    let gh = MockGh::default();
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let in_flight = Arc::new(DashSet::new());
    Box::pin(review_open_prs(
        &backend,
        "owner/repo",
        &default_config(),
        &tm,
        &store,
        &dispatching,
        &in_flight,
        &[snapshot],
        &gh,
        &router::RouterConfig::default(),
    ))
    .await
    .unwrap();

    let after = store.get(task_id).await.unwrap();
    assert_eq!(after.status, TaskStatus::Blocked);
    assert!(
        after.block_reason.is_some(),
        "block_reason must be set atomically with Blocked status"
    );
}

// ─── review_open_prs: Phase 3 — transient error skips without increment ──

#[tokio::test]
async fn review_open_prs_transient_error_in_is_pr_merged_skips_task() {
    struct TransientGh;

    #[async_trait]
    impl GhReviewClient for TransientGh {
        async fn get_pr_number(&self, _repo: &str, _branch: &str) -> anyhow::Result<Option<u64>> {
            Ok(None)
        }
        async fn is_pr_merged(&self, _repo: &str, _branch: &str) -> anyhow::Result<bool> {
            anyhow::bail!("500 Internal Server Error (transient GitHub 5xx)")
        }
        async fn batch_fetch_pr_review_data(
            &self,
            _repo: &str,
            _pr_numbers: &[u64],
        ) -> anyhow::Result<HashMap<u64, PrReviewBatchData>> {
            Ok(HashMap::new())
        }
        async fn is_collaborator(&self, _repo: &str, _username: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
    }

    let store = make_store().await;
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend);
    let tm = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "task4".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let _ = store::store_set_by_id(
        Some(&store),
        task_id,
        &[("branch", serde_json::json!("feat-branch"))],
    )
    .await;
    store
        .update_status(task_id, TaskStatus::InReview)
        .await
        .unwrap();
    let stored = store.get(task_id).await.unwrap();
    let snapshot = cleanup::ReviewTaskSnapshot {
        external: tasks::store_task_to_external(&stored),
        stored,
    };

    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let in_flight = Arc::new(DashSet::new());
    Box::pin(review_open_prs(
        &backend,
        "owner/repo",
        &default_config(),
        &tm,
        &store,
        &dispatching,
        &in_flight,
        &[snapshot],
        &TransientGh,
        &router::RouterConfig::default(),
    ))
    .await
    .unwrap();

    // Status must remain InReview and counter must not have been incremented.
    let after = store.get(task_id).await.unwrap();
    assert_eq!(after.status, TaskStatus::InReview);
    assert_eq!(after.no_code_reroutes, 0);
}

// ─── review_open_prs: Phase 4 — batch fetch failure → graceful skip ──────

#[tokio::test]
async fn review_open_prs_batch_fetch_failure_returns_ok() {
    let store = make_store().await;
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend);
    let tm = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "task5".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let _ = store::store_set_by_id(
        Some(&store),
        task_id,
        &[("branch", serde_json::json!("feat-branch"))],
    )
    .await;
    // Set pr_number so task reaches Phase 4.
    let _ = store::store_set_by_id(
        Some(&store),
        task_id,
        &[("pr_number", serde_json::json!(99i64))],
    )
    .await;
    store
        .update_status(task_id, TaskStatus::InReview)
        .await
        .unwrap();
    let stored = store.get(task_id).await.unwrap();
    let snapshot = cleanup::ReviewTaskSnapshot {
        external: tasks::store_task_to_external(&stored),
        stored,
    };

    let gh = MockGh {
        batch_error: true,
        ..Default::default()
    };
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let in_flight = Arc::new(DashSet::new());
    let result = Box::pin(review_open_prs(
        &backend,
        "owner/repo",
        &default_config(),
        &tm,
        &store,
        &dispatching,
        &in_flight,
        &[snapshot],
        &gh,
        &router::RouterConfig::default(),
    ))
    .await;

    // Must return Ok (graceful skip) even though batch fetch failed.
    assert!(result.is_ok());
    // Task status must be unchanged.
    let after = store.get(task_id).await.unwrap();
    assert_eq!(after.status, TaskStatus::InReview);
}

#[tokio::test]
async fn review_open_prs_does_not_merge_an_in_review_task_on_a_stale_approve_comment() {
    let store = make_store().await;
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend);
    let tm = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "stale-approve".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let _ = store::store_set_by_id(
        Some(&store),
        task_id,
        &[
            ("branch", serde_json::json!("feat-stale")),
            ("pr_number", serde_json::json!(88i64)),
        ],
    )
    .await;
    store
        .update_status(task_id, TaskStatus::InReview)
        .await
        .unwrap();
    let stored = store.get(task_id).await.unwrap();
    let snapshot = cleanup::ReviewTaskSnapshot {
        external: tasks::store_task_to_external(&stored),
        stored,
    };
    let task_key = snapshot.external.id.0.clone();

    let comment = make_comment(
        1,
        "bot",
        "## Automated Review \u{2014} Approve\n\nLooks good.",
        "2026-01-01T00:00:00Z",
    );
    let mut gh = MockGh::default();
    gh.collaborators.insert("bot".to_string(), true);
    gh.batch_data.insert(
        88,
        PrReviewBatchData {
            merged: false,
            head_sha: "new-head".to_string(),
            mergeable: Some(true),
            reviews: vec![],
            review_comments: vec![],
            issue_comments: vec![comment],
        },
    );

    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let in_flight = Arc::new(DashSet::new());
    Box::pin(review_open_prs(
        &backend,
        "owner/repo",
        &default_config(),
        &tm,
        &store,
        &dispatching,
        &in_flight,
        &[snapshot],
        &gh,
        &router::RouterConfig::default(),
    ))
    .await
    .unwrap();

    let after = store.get(task_id).await.unwrap();
    assert_eq!(after.status, TaskStatus::InReview);
    assert!(
        !in_flight.contains(&task_key),
        "an in_review task must not spawn auto_merge_pr on a leftover Approve comment"
    );
    assert_eq!(after.merge_conflict_retries, 0);
}

#[tokio::test]
async fn review_open_prs_leaves_conflict_retries_to_auto_merge() {
    let store = make_store().await;
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend);
    let tm = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "task-conflict".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let _ = store::store_set_by_id(
        Some(&store),
        task_id,
        &[
            ("branch", serde_json::json!("feat-conflict")),
            ("pr_number", serde_json::json!(77i64)),
        ],
    )
    .await;
    store
        .update_status(task_id, TaskStatus::Approved)
        .await
        .unwrap();
    let stored = store.get(task_id).await.unwrap();
    let snapshot = cleanup::ReviewTaskSnapshot {
        external: tasks::store_task_to_external(&stored),
        stored,
    };
    let task_key = snapshot.external.id.0.clone();

    let mut gh = MockGh::default();
    gh.batch_data.insert(
        77,
        PrReviewBatchData {
            merged: false,
            head_sha: String::new(),
            mergeable: Some(false),
            reviews: vec![make_review(
                1,
                "reviewer",
                "APPROVED",
                "2026-01-01T00:00:00Z",
                None,
            )],
            review_comments: vec![],
            issue_comments: vec![],
        },
    );

    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let in_flight = Arc::new(DashSet::new());
    Box::pin(review_open_prs(
        &backend,
        "owner/repo",
        &default_config(),
        &tm,
        &store,
        &dispatching,
        &in_flight,
        &[snapshot],
        &gh,
        &router::RouterConfig::default(),
    ))
    .await
    .unwrap();

    let after = store.get(task_id).await.unwrap();
    assert_eq!(after.status, TaskStatus::Approved);
    assert_eq!(
        after.merge_conflict_retries, 0,
        "review_poll must not increment merge_conflict_retries, auto_merge_pr owns the conflict"
    );
    assert!(
        in_flight.contains(&task_key),
        "an approved conflicting PR must spawn auto_merge_pr once"
    );
}

#[test]
fn reset_batch_fail_counter_delete_failure_falls_back_to_zero_and_logs_warning() {
    let key = "review_batch_fail:owner/repo";
    let calls: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let store = MockBatchFailStore {
        delete_error: Some("simulated delete failure".to_string()),
        set_error: None,
        set_calls: Arc::clone(&calls),
    };

    let logs = with_captured_warn_logs(|| {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        let ok = rt.block_on(reset_batch_fail_counter(&store, key));
        assert!(ok, "fallback zero-reset should succeed");
    });

    let set_calls = calls.lock().unwrap();
    assert_eq!(set_calls.len(), 1);
    assert_eq!(set_calls[0], (key.to_string(), "0".to_string()));
    assert!(
        logs.contains("falling back to zero reset"),
        "expected warning log to mention fallback reset, got: {logs}"
    );
}

#[test]
fn reset_batch_fail_counter_delete_and_set_failure_logs_error() {
    let key = "review_batch_fail:owner/repo";
    let calls: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let store = MockBatchFailStore {
        delete_error: Some("simulated delete failure".to_string()),
        set_error: Some("simulated set failure".to_string()),
        set_calls: Arc::clone(&calls),
    };

    let logs = with_captured_warn_logs(|| {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        let ok = rt.block_on(reset_batch_fail_counter(&store, key));
        assert!(
            !ok,
            "double failure should be surfaced as unsuccessful reset"
        );
    });

    let set_calls = calls.lock().unwrap();
    assert_eq!(set_calls.len(), 1);
    assert!(
        logs.contains("failed to reset review batch failure counter"),
        "expected error log when both delete and set fail, got: {logs}"
    );
}

// ─── review_open_prs: watermark dedup — review_ts_map skips old reviews ──

#[tokio::test]
async fn review_open_prs_watermark_dedup_skips_already_processed_review() {
    let store = make_store().await;
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend);
    let tm = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "task6".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    // Pre-set review_ts_map so the reviewer's review is already watermarked.
    let ts = "2026-01-05T00:00:00Z";
    let _ = store::store_set_by_id(
        Some(&store),
        task_id,
        &[
            ("branch", serde_json::json!("feat-branch")),
            ("pr_number", serde_json::json!(10i64)),
            (
                "review_ts_map",
                serde_json::json!(serde_json::json!({"reviewer1": ts}).to_string()),
            ),
        ],
    )
    .await;
    store
        .update_status(task_id, TaskStatus::InReview)
        .await
        .unwrap();
    let stored = store.get(task_id).await.unwrap();
    let snapshot = cleanup::ReviewTaskSnapshot {
        external: tasks::store_task_to_external(&stored),
        stored,
    };

    // Batch data has a CHANGES_REQUESTED review at the same timestamp as the watermark.
    let review = make_review(1, "reviewer1", "CHANGES_REQUESTED", ts, Some("Fix this"));
    let batch_data = PrReviewBatchData {
        merged: false,
        head_sha: String::new(),
        mergeable: Some(true),
        reviews: vec![review],
        review_comments: vec![],
        issue_comments: vec![],
    };
    let mut gh = MockGh::default();
    gh.batch_data.insert(10, batch_data);

    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let in_flight = Arc::new(DashSet::new());
    Box::pin(review_open_prs(
        &backend,
        "owner/repo",
        &default_config(),
        &tm,
        &store,
        &dispatching,
        &in_flight,
        &[snapshot],
        &gh,
        &router::RouterConfig::default(),
    ))
    .await
    .unwrap();

    // Status must remain InReview — review was skipped due to watermark.
    let after = store.get(task_id).await.unwrap();
    assert_eq!(after.status, TaskStatus::InReview);
}

#[tokio::test]
async fn review_open_prs_does_not_rewrite_stored_pr_number() {
    let store = make_store().await;
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend);
    let tm = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "task-pr-number".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let _ = store::store_set_by_id(
        Some(&store),
        task_id,
        &[
            ("branch", serde_json::json!("feat-branch")),
            ("pr_number", serde_json::json!(42i64)),
        ],
    )
    .await;
    store
        .update_status(task_id, TaskStatus::InReview)
        .await
        .unwrap();
    let stored = store.get(task_id).await.unwrap();
    let snapshot = cleanup::ReviewTaskSnapshot {
        external: tasks::store_task_to_external(&stored),
        stored,
    };
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(task_id)
        .execute(store.pool())
        .await
        .unwrap();

    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let in_flight = Arc::new(DashSet::new());
    Box::pin(review_open_prs(
        &backend,
        "owner/repo",
        &default_config(),
        &tm,
        &store,
        &dispatching,
        &in_flight,
        &[snapshot],
        &MockGh::default(),
        &router::RouterConfig::default(),
    ))
    .await
    .unwrap();

    let after = store.get(task_id).await.unwrap();
    assert_eq!(after.pr_number, Some(42));
    assert_eq!(
        after.updated_at, "2020-01-01T00:00:00Z",
        "rewriting an already-stored pr_number must not bump updated_at"
    );
}

// ─── review_open_prs: last_comment_review_ts dedup ───────────────────────

#[tokio::test]
async fn review_open_prs_last_comment_review_ts_dedup_skips_old_comment() {
    let store = make_store().await;
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend);
    let tm = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));

    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "task7".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let _ = store::store_set_by_id(
        Some(&store),
        task_id,
        &[("branch", serde_json::json!("feat-branch"))],
    )
    .await;
    let _ = store::store_set_by_id(
        Some(&store),
        task_id,
        &[("pr_number", serde_json::json!(20i64))],
    )
    .await;
    // Set last_comment_review_ts to match the comment's created_at so it is deduplicated.
    let ts = "2026-01-05T00:00:00Z";
    let _ = store::store_set_by_id(
        Some(&store),
        task_id,
        &[("last_comment_review_ts", serde_json::json!(ts))],
    )
    .await;
    store
        .update_status(task_id, TaskStatus::InReview)
        .await
        .unwrap();
    let stored = store.get(task_id).await.unwrap();
    let snapshot = cleanup::ReviewTaskSnapshot {
        external: tasks::store_task_to_external(&stored),
        stored,
    };

    let comment_body = "## Automated Review \u{2014} Changes Requested\n\nFix the bug.";
    let comment = make_comment(1, "bot", comment_body, ts);
    let batch_data = PrReviewBatchData {
        merged: false,
        head_sha: String::new(),
        mergeable: Some(true),
        reviews: vec![],
        review_comments: vec![],
        issue_comments: vec![comment],
    };
    let mut gh = MockGh::default();
    gh.batch_data.insert(20, batch_data);
    gh.collaborators.insert("bot".to_string(), true);

    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let in_flight = Arc::new(DashSet::new());
    Box::pin(review_open_prs(
        &backend,
        "owner/repo",
        &default_config(),
        &tm,
        &store,
        &dispatching,
        &in_flight,
        &[snapshot],
        &gh,
        &router::RouterConfig::default(),
    ))
    .await
    .unwrap();

    // Status must remain InReview — comment was deduplicated.
    let after = store.get(task_id).await.unwrap();
    assert_eq!(after.status, TaskStatus::InReview);
}

// ─── review_open_prs: review context truncation ──────────────────────────

#[test]
fn review_context_truncation_at_16kb_boundary() {
    // Replicate the truncation logic from review_open_prs to verify it works correctly.
    const MAX_REVIEW_CONTEXT_BYTES: usize = 16 * 1024;

    // Build a review_context that exceeds 16 KB.
    let repeated = "a".repeat(200);
    let mut review_context = String::new();
    while review_context.len() <= MAX_REVIEW_CONTEXT_BYTES {
        review_context.push_str(&repeated);
        review_context.push('\n');
    }

    assert!(review_context.len() > MAX_REVIEW_CONTEXT_BYTES);

    // Apply the same truncation logic as in review_open_prs.
    if review_context.len() > MAX_REVIEW_CONTEXT_BYTES {
        let mut boundary = MAX_REVIEW_CONTEXT_BYTES;
        while !review_context.is_char_boundary(boundary) {
            boundary -= 1;
        }
        if let Some(pos) = review_context[..boundary].rfind('\n') {
            review_context.truncate(pos);
        } else {
            review_context.truncate(boundary);
        }
        review_context.push_str("\n... (review context truncated)");
    }

    assert!(review_context.len() <= MAX_REVIEW_CONTEXT_BYTES + 40); // suffix is short
    assert!(review_context.ends_with("... (review context truncated)"));
    // Must be valid UTF-8 after truncation (no split multi-byte chars).
    assert!(str::from_utf8(review_context.as_bytes()).is_ok());
}
