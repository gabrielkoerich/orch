use super::comments::{CommentAction, MentionCursor, classify_comment, handle_slash_command};
use super::unblock::{
    ci_failure_unblock_cooldown_elapsed, is_ci_failure_block, try_unblock_ci_failure_task,
};
use super::*;
use crate::backends::test_helpers::NoopBackend;
use crate::backends::{ExternalId, ExternalTask, Mention, Status};
use crate::engine::auto_merge::{
    CI_FAILURE_AUTO_MERGE_STAGE, CI_FAILURE_BEFORE_REVIEW_STAGE, ci_failure_limit_reason,
};
use crate::engine::commands::CommandGh;
use crate::engine::commands::OwnerCommand;
use crate::engine::cooldown::record_persistent_model_failure;
use crate::engine::cooldown::set_agent_cooldown;
use crate::engine::dispatch_guard::DispatchGuard;
use crate::engine::router::Router;
use crate::engine::router::RouterConfig;
use crate::github::token::ENV_LOCK;
use crate::github::types::GitHubIssue;
use crate::github::types::GitHubUser;
use crate::store::NewTask;
use crate::store::{RunTokenUsage, TaskStore};
use async_trait::async_trait;
use serial_test::serial;
use std::env;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ── ingest_external_tasks tests ─────────────────────────────────────────

/// Mock backend that returns configurable tasks per status.
struct IngestMockBackend {
    /// Stored as (status_label, tasks) pairs since Status doesn't impl Hash.
    tasks: Vec<(String, Vec<ExternalTask>)>,
    dedup_result: bool,
    comments: Mutex<Vec<(String, String)>>,
    status_updates: Mutex<Vec<(String, Status)>>,
    project_syncs: Mutex<Vec<(String, Status)>>,
    project_reconciliations: AtomicUsize,
    /// Counts `list_active_open_issues` calls.
    list_calls: AtomicUsize,
}

impl IngestMockBackend {
    fn with_tasks(tasks: Vec<(Status, ExternalTask)>) -> Arc<Self> {
        Self::with_tasks_and_dedup(tasks, false)
    }

    fn with_tasks_and_dedup(tasks: Vec<(Status, ExternalTask)>, dedup_result: bool) -> Arc<Self> {
        let mut grouped: Vec<(String, Vec<ExternalTask>)> = Vec::new();
        for (status, task) in tasks {
            let label = status.as_label().to_string();
            if let Some(entry) = grouped.iter_mut().find(|(l, _)| l == &label) {
                entry.1.push(task);
            } else {
                grouped.push((label, vec![task]));
            }
        }
        Arc::new(Self {
            tasks: grouped,
            dedup_result,
            comments: Mutex::new(Vec::new()),
            status_updates: Mutex::new(Vec::new()),
            project_syncs: Mutex::new(Vec::new()),
            project_reconciliations: AtomicUsize::new(0),
            list_calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl ExternalBackend for IngestMockBackend {
    fn name(&self) -> &'static str {
        "ingest-mock"
    }
    async fn create_task(&self, _: &str, _: &str, _: &[String]) -> anyhow::Result<ExternalId> {
        Ok(ExternalId("new".into()))
    }
    async fn get_task(&self, _: &ExternalId) -> anyhow::Result<ExternalTask> {
        anyhow::bail!("not implemented")
    }
    async fn list_by_status(&self, status: Status) -> anyhow::Result<Vec<ExternalTask>> {
        let label = status.as_label().to_string();
        Ok(self
            .tasks
            .iter()
            .find(|(l, _)| l == &label)
            .map(|(_, t)| t.clone())
            .unwrap_or_default())
    }
    async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
        Ok(vec![])
    }
    async fn list_active_open_issues(&self) -> anyhow::Result<Vec<(ExternalTask, Option<Status>)>> {
        self.list_calls.fetch_add(1, Ordering::Relaxed);
        let mut result = Vec::new();
        for (label, tasks) in &self.tasks {
            let status = match label.as_str() {
                "status:routed" => Some(Status::Routed),
                "status:in_progress" => Some(Status::InProgress),
                "status:needs_review" => Some(Status::NeedsReview),
                "status:in_review" => Some(Status::InReview),
                "status:approved" => Some(Status::Approved),
                "status:blocked" => Some(Status::Blocked),
                _ => None,
            };
            for task in tasks {
                result.push((task.clone(), status));
            }
        }
        Ok(result)
    }
    async fn post_comment(&self, id: &ExternalId, body: &str) -> anyhow::Result<()> {
        self.comments
            .lock()
            .await
            .push((id.0.clone(), body.to_string()));
        Ok(())
    }
    async fn set_labels(&self, _: &ExternalId, _: &[String]) -> anyhow::Result<()> {
        Ok(())
    }
    async fn remove_label(&self, _: &ExternalId, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn get_sub_issues(&self, _: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
        Ok(vec![])
    }
    async fn health_check(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn get_mentions(&self, _: &str) -> anyhow::Result<Vec<Mention>> {
        Ok(vec![])
    }
    async fn update_status(&self, id: &ExternalId, status: Status) -> anyhow::Result<()> {
        self.status_updates
            .lock()
            .await
            .push((id.0.clone(), status));
        Ok(())
    }

    async fn has_open_issue_with_title(&self, _: &str, _: &str) -> anyhow::Result<bool> {
        Ok(self.dedup_result)
    }

    async fn sync_to_project(
        &self,
        id: &ExternalId,
        status: Status,
    ) -> anyhow::Result<Option<String>> {
        self.project_syncs.lock().await.push((id.0.clone(), status));
        Ok(None) // Mock doesn't have real node IDs.
    }

    async fn reconcile_project_statuses(&self) -> anyhow::Result<()> {
        self.project_reconciliations.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn make_ext_task(id: &str, title: &str) -> ExternalTask {
    ExternalTask {
        id: ExternalId(id.to_string()),
        title: title.to_string(),
        body: String::new(),
        state: "open".to_string(),
        labels: vec![],
        author: "user".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: format!("https://github.com/owner/repo/issues/{id}"),
    }
}

#[serial(cooldown_state)]
#[tokio::test]
async fn ingest_upserts_tasks_into_store() {
    cooldown::reset_global_state().await;
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![
        (Status::New, make_ext_task("1", "First issue")),
        (Status::InProgress, make_ext_task("2", "Second issue")),
    ]);
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    ingest_external_tasks(&backend, "owner/repo", &store)
        .await
        .unwrap();

    // Both tasks should be in the store
    let all = store.list_all("owner/repo").await.unwrap();
    assert_eq!(all.len(), 2);

    let task1 = store
        .get_by_external_id("owner/repo", "1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task1.title, "First issue");
    assert_eq!(task1.status, TaskStatus::New);

    let task2 = store
        .get_by_external_id("owner/repo", "2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task2.title, "Second issue");
    assert_eq!(task2.status, TaskStatus::InProgress);
}

#[serial(cooldown_state)]
#[tokio::test]
async fn ingest_takes_every_open_issue_whatever_its_age_or_the_last_ingest() {
    // An issue opened after a restart reaches the store on the first sync, and so does
    // one whose updated_at predates any window a `since` cursor would have used. The
    // ingest keeps no cursor, so a stale KV value from an older build changes nothing.
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    store
        .kv_set("issues_last_ingested:owner/repo", "2026-10-26T00:00:00Z")
        .await
        .unwrap();

    let mut old = make_ext_task("73", "Filed weeks ago, never ingested");
    old.updated_at = "2026-01-01T00:00:00Z".to_string();
    let backend = IngestMockBackend::with_tasks(vec![(Status::New, old)]);
    let backend_dyn: Arc<dyn ExternalBackend> = Arc::<IngestMockBackend>::clone(&backend);
    ingest_external_tasks(&backend_dyn, "owner/repo", &store)
        .await
        .unwrap();
    assert!(
        store
            .get_by_external_id("owner/repo", "73")
            .await
            .unwrap()
            .is_some()
    );

    // A second sync, standing in for the one right after a restart, sees a new issue.
    let backend = IngestMockBackend::with_tasks(vec![(
        Status::New,
        make_ext_task("78", "Opened after the restart"),
    )]);
    let backend_dyn: Arc<dyn ExternalBackend> = Arc::<IngestMockBackend>::clone(&backend);
    ingest_external_tasks(&backend_dyn, "owner/repo", &store)
        .await
        .unwrap();
    assert_eq!(
        backend.list_calls.load(Ordering::Relaxed),
        1,
        "one sync is one listing"
    );
    assert!(
        store
            .get_by_external_id("owner/repo", "78")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        store
            .kv_get("issues_last_ingested:owner/repo")
            .await
            .unwrap()
            .as_deref(),
        Some("2026-10-26T00:00:00Z"),
        "the ingest must keep no cursor, so a stale one is never read or advanced"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn ingest_updates_existing_tasks() {
    cooldown::reset_global_state().await;
    let backend: Arc<dyn ExternalBackend> =
        IngestMockBackend::with_tasks(vec![(Status::Routed, make_ext_task("42", "Updated title"))]);
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    // Pre-create the task
    store
        .create(&NewTask {
            external_id: Some("42".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Original title".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    ingest_external_tasks(&backend, "owner/repo", &store)
        .await
        .unwrap();

    // Should have updated the title
    let task = store
        .get_by_external_id("owner/repo", "42")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.title, "Updated title");
    assert_eq!(task.status, TaskStatus::Routed);
}

#[serial(cooldown_state)]
#[tokio::test]
async fn ingest_closes_duplicate_titles() {
    cooldown::reset_global_state().await;

    let backend = IngestMockBackend::with_tasks_and_dedup(
        vec![(Status::New, make_ext_task("2", "Duplicate issue"))],
        true,
    );
    let backend_dyn: Arc<dyn ExternalBackend> = Arc::<IngestMockBackend>::clone(&backend);
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    store
        .create(&NewTask {
            external_id: Some("1".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Duplicate issue".to_string(),
            labels: vec!["status:new".to_string()],
            ..Default::default()
        })
        .await
        .unwrap();

    ingest_external_tasks(&backend_dyn, "owner/repo", &store)
        .await
        .unwrap();

    let all = store.list_all("owner/repo").await.unwrap();
    assert_eq!(all.len(), 1);

    let comments = backend.comments.lock().await;
    assert_eq!(comments.len(), 1);
    assert!(comments[0].1.contains("Duplicate of #1"));

    let status_updates = backend.status_updates.lock().await;
    assert_eq!(status_updates.len(), 1);
    assert_eq!(status_updates[0].0, "2");
    assert_eq!(status_updates[0].1, Status::Done);
}

#[serial(cooldown_state)]
#[tokio::test]
async fn ingest_handles_empty_backend() {
    cooldown::reset_global_state().await;
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    // Should not error on empty backend
    let result = ingest_external_tasks(&backend, "owner/repo", &store).await;
    assert!(result.is_ok());

    let all = store.list_all("owner/repo").await.unwrap();
    assert!(all.is_empty());
}

#[serial(cooldown_state)]
#[tokio::test]
async fn ingest_syncs_status_correctly_across_statuses() {
    cooldown::reset_global_state().await;
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![
        (Status::New, make_ext_task("1", "New")),
        (Status::Routed, make_ext_task("2", "Routed")),
        (Status::InProgress, make_ext_task("3", "InProgress")),
        (Status::NeedsReview, make_ext_task("4", "NeedsReview")),
        (Status::InReview, make_ext_task("5", "InReview")),
        (Status::Blocked, make_ext_task("6", "Blocked")),
    ]);
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    ingest_external_tasks(&backend, "owner/repo", &store)
        .await
        .unwrap();

    let counts = store.status_counts("owner/repo").await.unwrap();
    assert_eq!(counts.get("new"), Some(&1));
    assert_eq!(counts.get("routed"), Some(&1));
    assert_eq!(counts.get("in_progress"), Some(&1));
    assert_eq!(counts.get("needs_review"), Some(&1));
    assert_eq!(counts.get("in_review"), Some(&1));
    assert_eq!(counts.get("blocked"), Some(&1));
}

#[serial(cooldown_state)]
#[tokio::test]
async fn ingest_does_not_overwrite_store_authoritative_status() {
    cooldown::reset_global_state().await;
    // Backend reports task as New (GitHub labels haven't caught up yet)
    let backend: Arc<dyn ExternalBackend> =
        IngestMockBackend::with_tasks(vec![(Status::New, make_ext_task("99", "My task"))]);
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    // Pre-create the task and advance it to Routed in the store
    let id = store
        .create(&NewTask {
            external_id: Some("99".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "My task".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Routed).await.unwrap();

    // Ingest should NOT overwrite Routed back to New
    ingest_external_tasks(&backend, "owner/repo", &store)
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::Routed,
        "ingest must not overwrite store-authoritative status"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn ingest_sync_to_project_called_only_for_new_tasks() {
    cooldown::reset_global_state().await;

    // Two tasks: "10" is pre-existing, "11" is new.
    let backend = IngestMockBackend::with_tasks(vec![
        (Status::New, make_ext_task("10", "Existing task")),
        (Status::New, make_ext_task("11", "New task")),
    ]);
    let backend_dyn: Arc<dyn ExternalBackend> = Arc::<IngestMockBackend>::clone(&backend);
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    // Pre-insert task "10" so it already exists in the store.
    store
        .create(&NewTask {
            external_id: Some("10".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Existing task".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    ingest_external_tasks(&backend_dyn, "owner/repo", &store)
        .await
        .unwrap();

    let syncs = backend.project_syncs.lock().await;
    // Only the new task "11" should trigger sync_to_project — not the pre-existing "10".
    assert_eq!(
        syncs.len(),
        1,
        "sync_to_project must be called exactly once (for new task only), got: {syncs:?}"
    );
    assert_eq!(syncs[0].0, "11", "sync_to_project must target the new task");
}

#[serial(cooldown_state)]
#[tokio::test]
async fn ingest_handles_mixed_labeled_and_unlabeled_open_issues() {
    // Simulate a backend where some issues have no status label (routable/unlabeled)
    // and others carry active status labels.  Both kinds must be ingested and the
    // store status must reflect the label where present.
    struct MixedBackend;

    #[async_trait]
    impl ExternalBackend for MixedBackend {
        fn name(&self) -> &'static str {
            "mixed"
        }
        async fn create_task(&self, _: &str, _: &str, _: &[String]) -> anyhow::Result<ExternalId> {
            Ok(ExternalId("x".into()))
        }
        async fn get_task(&self, _: &ExternalId) -> anyhow::Result<ExternalTask> {
            anyhow::bail!("not implemented")
        }
        async fn list_by_status(&self, _: Status) -> anyhow::Result<Vec<ExternalTask>> {
            Ok(vec![])
        }
        /// Return a mix: one unlabeled (no status) + one labeled in_progress.
        async fn list_active_open_issues(
            &self,
        ) -> anyhow::Result<Vec<(ExternalTask, Option<Status>)>> {
            Ok(vec![
                (make_ext_task("100", "Unlabeled routable"), None),
                (
                    make_ext_task("101", "In progress issue"),
                    Some(Status::InProgress),
                ),
            ])
        }
        async fn post_comment(&self, _: &ExternalId, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn set_labels(&self, _: &ExternalId, _: &[String]) -> anyhow::Result<()> {
            Ok(())
        }
        async fn remove_label(&self, _: &ExternalId, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn get_sub_issues(&self, _: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
            Ok(vec![])
        }
        async fn health_check(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn get_mentions(&self, _: &str) -> anyhow::Result<Vec<Mention>> {
            Ok(vec![])
        }
        async fn update_status(&self, _: &ExternalId, _: Status) -> anyhow::Result<()> {
            Ok(())
        }
        async fn has_open_issue_with_title(&self, _: &str, _: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn sync_to_project(
            &self,
            _: &ExternalId,
            _: Status,
        ) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
    }

    cooldown::reset_global_state().await;

    let backend: Arc<dyn ExternalBackend> = Arc::new(MixedBackend);
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    ingest_external_tasks(&backend, "owner/repo", &store)
        .await
        .unwrap();

    let all = store.list_all("owner/repo").await.unwrap();
    assert_eq!(all.len(), 2, "both issues must be ingested");

    let unlabeled = store
        .get_by_external_id("owner/repo", "100")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        unlabeled.status,
        TaskStatus::New,
        "unlabeled issue should start as New"
    );

    let in_progress = store
        .get_by_external_id("owner/repo", "101")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        in_progress.status,
        TaskStatus::InProgress,
        "labeled in_progress issue must be synced to InProgress"
    );
}

// ── kv_get_prefer_store / kv_set_prefer_store ────────────────────

#[serial(cooldown_state)]
#[tokio::test]
async fn kv_get_prefer_store_reads_from_store() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    store.kv_set("k1", "store_val").await.unwrap();

    let opt = Some(&store);
    let val = kv_get_prefer_store(opt, "k1").await;
    assert_eq!(val.as_deref(), Some("store_val"));
}

#[serial(cooldown_state)]
#[tokio::test]
async fn kv_get_prefer_store_returns_none_without_store() {
    cooldown::reset_global_state().await;
    let opt: Option<&Arc<TaskStore>> = None;
    let val = kv_get_prefer_store(opt, "k2").await;
    assert_eq!(val, None);
}

#[serial(cooldown_state)]
#[tokio::test]
async fn kv_set_prefer_store_writes_to_store() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    let opt = Some(&store);
    kv_set_prefer_store(opt, "k3", "val3").await;

    assert_eq!(store.kv_get("k3").await.unwrap().as_deref(), Some("val3"));
}

#[serial(cooldown_state)]
#[tokio::test]
async fn kv_set_prefer_store_noop_without_store() {
    cooldown::reset_global_state().await;
    let opt: Option<&Arc<TaskStore>> = None;
    // Should not panic
    kv_set_prefer_store(opt, "k4", "val4").await;
}

// ── try_kv_get_prefer_store / try_kv_set_prefer_store ────────────────

#[serial(cooldown_state)]
#[tokio::test]
async fn try_kv_get_prefer_store_returns_value_from_store() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    store.kv_set("try_k1", "try_val1").await.unwrap();

    let opt = Some(&store);
    let result = try_kv_get_prefer_store(opt, "try_k1").await;
    assert_eq!(result.unwrap().as_deref(), Some("try_val1"));
}

#[serial(cooldown_state)]
#[tokio::test]
async fn try_kv_get_prefer_store_returns_none_for_missing_key() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let opt = Some(&store);

    let result = try_kv_get_prefer_store(opt, "nonexistent_key").await;
    assert_eq!(result.unwrap(), None); // Key doesn't exist, but query succeeded
}

#[serial(cooldown_state)]
#[tokio::test]
async fn try_kv_get_prefer_store_returns_none_without_store() {
    cooldown::reset_global_state().await;
    let opt: Option<&Arc<TaskStore>> = None;
    let result = try_kv_get_prefer_store(opt, "try_k2").await;
    assert_eq!(result.unwrap(), None);
}

#[serial(cooldown_state)]
#[tokio::test]
async fn try_kv_set_prefer_store_writes_to_store() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let opt = Some(&store);

    let result = try_kv_set_prefer_store(opt, "try_k3", "try_val3").await;
    assert!(result.is_ok());
    assert_eq!(
        store.kv_get("try_k3").await.unwrap().as_deref(),
        Some("try_val3")
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn try_kv_set_prefer_store_ok_without_store() {
    cooldown::reset_global_state().await;
    let opt: Option<&Arc<TaskStore>> = None;
    // Should not error - best effort when store unavailable
    let result = try_kv_set_prefer_store(opt, "try_k4", "try_val4").await;
    assert!(result.is_ok());
}

// ── dispatching lock tests ──────────────────────────────────────────

#[serial(cooldown_state)]
#[test]
fn dispatching_set_blocks_duplicate_processing() {
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

    let repo = "owner/repo";
    let task_id = "42";
    let dispatch_key = format!("{repo}/{task_id}");

    // Initially not in the map
    assert!(!dispatching.contains_key(&dispatch_key));

    // Insert — simulates dispatch starting
    dispatching.insert(dispatch_key.clone(), task_id.to_string());

    // Now should be blocked
    assert!(
        dispatching.contains_key(&dispatch_key),
        "task should be locked while dispatching"
    );

    // Remove — simulates review completion
    dispatching.remove(&dispatch_key);

    // Now should be free again
    assert!(
        !dispatching.contains_key(&dispatch_key),
        "task should be unlocked after review completes"
    );
}

#[serial(cooldown_state)]
#[test]
fn dispatching_set_does_not_block_other_tasks() {
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

    let repo = "owner/repo";

    // Lock task 42
    let key_42 = format!("{repo}/42");
    dispatching.insert(key_42.clone(), "42".to_string());

    // Task 43 should NOT be blocked
    let key_43 = format!("{repo}/43");
    assert!(
        dispatching.contains_key(&key_42),
        "task 42 should be locked"
    );
    assert!(
        !dispatching.contains_key(&key_43),
        "task 43 should not be locked"
    );
}

/// Regression test for a6d8b9a.
///
/// Before the fix, `sync_tick` step 5 transitioned a task to `InReview` and then
/// spawned a `tokio::spawn` review agent WITHOUT first inserting the `dispatch_key`
/// into the dispatching set. This created a window where `review_open_prs` (step 4)
/// in a concurrent sync tick could see the task as `InReview`, find the key absent,
/// and re-dispatch it — silently discarding human `CHANGES_REQUESTED` feedback when
/// the review agent later completed and marked the task `Done`.
///
/// The fix: insert `dispatch_key` BEFORE `tokio::spawn`, remove it AFTER the future
/// completes. This test verifies that invariant using tokio channels to synchronize
/// with a simulated concurrent caller (review_open_prs).
#[serial(cooldown_state)]
#[tokio::test]
async fn dispatch_key_held_during_review_agent_execution() {
    cooldown::reset_global_state().await;

    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

    let dispatch_key = "owner/repo/42".to_string();

    // Step 1: insert before spawning — this is the invariant added by a6d8b9a.
    dispatching.insert(dispatch_key.clone(), "42".to_string());

    let (agent_started_tx, agent_started_rx) = oneshot::channel::<()>();
    let (check_done_tx, check_done_rx) = oneshot::channel::<()>();

    let dispatching_agent = Arc::clone(&dispatching);
    let key_agent = dispatch_key.clone();

    // Step 2: spawn review agent — key is ALREADY in the set (fix invariant holds).
    let review_task = tokio::spawn(async move {
        agent_started_tx.send(()).unwrap();
        // Simulate review agent work — hold until the concurrent check is done.
        check_done_rx.await.unwrap();
        // Remove from dispatching set on completion (mirror of tick.rs/sync.rs).
        dispatching_agent.remove(&key_agent);
    });

    // Step 3: concurrent review_open_prs check — simulates step 4 of another
    // sync_tick invocation running while the review agent is still active.
    agent_started_rx.await.unwrap();
    assert!(
        dispatching.contains_key(&dispatch_key),
        "dispatch_key must be visible in the dispatching set while the review \
             agent is running — without the a6d8b9a fix, review_open_prs would see \
             the key absent and re-dispatch the task, silently dropping \
             CHANGES_REQUESTED feedback"
    );

    // Step 4: review agent completes.
    check_done_tx.send(()).unwrap();
    review_task.await.unwrap();

    // Step 5: key released — review_open_prs can now act on the task if needed.
    assert!(
        !dispatching.contains_key(&dispatch_key),
        "dispatch_key must be removed from the dispatching set after the \
             review agent completes so subsequent sync ticks can process the task"
    );
}

/// Regression test for issue #833.
///
/// Before the fix, `dispatch_key` was inserted into the `dispatching` HashSet
/// *outside* the spawned task but removed *inside* it.  When the spawned async
/// block panicked (e.g. an `unwrap()` inside `review_and_merge`), Tokio caught
/// the panic and terminated the task without running the cleanup code, leaving
/// the key in the set permanently.  Subsequent stuck-task recovery would reset
/// the task to `NeedsReview`, but the subscriber/sync would see the key still
/// present and skip it forever — a permanent review loop until service restart.
///
/// The fix introduces `DispatchGuard`, a RAII wrapper whose `Drop` impl removes
/// the key unconditionally, even when the task unwinds via panic.
#[serial(cooldown_state)]
#[tokio::test]
async fn dispatch_guard_releases_key_on_panic() {
    cooldown::reset_global_state().await;

    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let key = "owner/repo/42".to_string();

    // Insert key before spawn — mirrors the production invariant established
    // by the a6d8b9a fix (key must be visible before the spawn so concurrent
    // review_open_prs callers see it and skip the task).
    dispatching.insert(key.clone(), "42".to_string());

    // Create the guard (takes ownership of the removal obligation).
    let guard = DispatchGuard::new(Arc::clone(&dispatching), key.clone());

    // Spawn a task that panics before the normal completion path.
    let handle = tokio::spawn(async move {
        let _guard = guard; // moved in; Drop removes key whether we panic or not
        panic!("simulated panic inside review_and_merge");
    });

    // Absorb the JoinError — the panic is expected.
    let _ = handle.await;

    // Key MUST be gone even though the task panicked.
    assert!(
        !dispatching.contains_key(&key),
        "dispatch key must be removed from the dispatching set even when the \
             spawned review task panics — without DispatchGuard the key leaks and \
             the task gets stuck in a permanent review loop"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn sync_tick_reconciles_board_without_ingesting_or_transitioning_tasks() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = IngestMockBackend::with_tasks(vec![]);
    let backend: Arc<dyn ExternalBackend> = Arc::<IngestMockBackend>::clone(&mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let dispatching = Arc::new(DashMap::new());
    Box::pin(sync_tick(
        &backend,
        &Arc::new(TmuxManager::new()),
        "owner/repo",
        &EngineConfig::default(),
        &Arc::new(RwLock::new(Router::new_for_test(
            RouterConfig::default(),
            vec!["claude".to_string()],
        ))),
        &task_manager,
        &store,
        &dispatching,
        &Arc::new(DashSet::new()),
    ))
    .await
    .unwrap();
    assert_eq!(mock.project_reconciliations.load(Ordering::Relaxed), 1);
    assert!(mock.project_syncs.lock().await.is_empty());
    assert!(mock.status_updates.lock().await.is_empty());
    assert!(dispatching.is_empty());
    assert!(
        store
            .existing_external_ids("owner/repo")
            .await
            .unwrap()
            .is_empty()
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn needs_review_refire_increments_and_fires_when_old_enough() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let router = reviewer_router();
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "201",
            title: "Needs review old",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::NeedsReview)
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    let t = store.get(id).await.unwrap();
    assert_eq!(t.needs_review_refires, 0);

    refire_and_escalate_stale_needs_review_global(
        &task_manager,
        &store,
        &router,
        &dispatching,
        &sweep_repos(),
    )
    .await
    .unwrap();

    let t2 = store.get(id).await.unwrap();
    assert_eq!(t2.needs_review_refires, 1);
    assert_eq!(t2.status, TaskStatus::NeedsReview);
}

#[serial(cooldown_state)]
#[tokio::test]
async fn needs_review_backoff_delays_fire_but_increments_counter() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let router = reviewer_router();
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "202",
            title: "Needs review backoff",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::NeedsReview)
        .await
        .unwrap();

    store
        .set_fields(id, &[("needs_review_refires", serde_json::json!(1))])
        .await
        .unwrap();

    // Age 1 minute: stale enough to consider, but backoff at refires=1 needs 2 minutes
    let now_minus_1 = (chrono::Utc::now() - chrono::Duration::minutes(1))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    sqlx::query("UPDATE tasks SET updated_at = ? WHERE id = ?")
        .bind(now_minus_1.clone())
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    let before = store.get(id).await.unwrap();
    assert_eq!(before.needs_review_refires, 1);

    refire_and_escalate_stale_needs_review_global(
        &task_manager,
        &store,
        &router,
        &dispatching,
        &sweep_repos(),
    )
    .await
    .unwrap();

    let after = store.get(id).await.unwrap();
    // Counter must not increment on a backoff delay, only on an actual re-fire
    assert_eq!(after.needs_review_refires, 1);
    assert_eq!(after.updated_at, now_minus_1);
    assert_eq!(after.status, TaskStatus::NeedsReview);
}

/// Router with one claude review model, so the refire sweeps see a reviewer.
fn reviewer_router() -> Router {
    let mut config = RouterConfig::default();
    config.model_map.insert(
        "review".to_string(),
        HashMap::from([("claude".to_string(), vec!["sonnet".to_string()])]),
    );
    Router::new_for_test(config, vec!["claude".to_string()])
}

#[serial(cooldown_state)]
#[tokio::test]
async fn needs_review_escalates_to_blocked_after_max_refires() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let router = reviewer_router();
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "203",
            title: "Needs review escalate",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::NeedsReview)
        .await
        .unwrap();

    // Counter at MAX (5): the next pass is attempt 6 and escalates
    store
        .set_fields(id, &[("needs_review_refires", serde_json::json!(5))])
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    refire_and_escalate_stale_needs_review_global(
        &task_manager,
        &store,
        &router,
        &dispatching,
        &sweep_repos(),
    )
    .await
    .unwrap();

    let after = store.get(id).await.unwrap();
    assert_eq!(after.status, TaskStatus::Blocked);
    assert!(after.block_reason.is_some());
}

/// Exactly MAX_NEEDS_REVIEW_REFIRE_ATTEMPTS refires are allowed before
/// escalation. Task stays NeedsReview at 5 refires and only blocks on the 6th
#[serial(cooldown_state)]
#[tokio::test]
async fn needs_review_allows_max_refires_before_escalation() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let router = reviewer_router();
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "204",
            title: "Max refires test",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::NeedsReview)
        .await
        .unwrap();

    // Counter at MAX-1 (4): incrementing hits MAX (5) and must not escalate
    store
        .set_fields(id, &[("needs_review_refires", serde_json::json!(4))])
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    refire_and_escalate_stale_needs_review_global(
        &task_manager,
        &store,
        &router,
        &dispatching,
        &sweep_repos(),
    )
    .await
    .unwrap();

    let after = store.get(id).await.unwrap();
    assert_eq!(after.status, TaskStatus::NeedsReview);
    assert_eq!(after.needs_review_refires, 5);
    assert!(after.block_reason.is_none());
}

/// Every repo the sweep tests use, so each counts as active
fn sweep_repos() -> HashSet<String> {
    ["owner/repo", "inactive/repo", "some/repo", "active/repo"]
        .into_iter()
        .map(String::from)
        .collect()
}

/// A repo outside the active set has no review subscriber (#4024)
#[serial(cooldown_state)]
#[tokio::test]
async fn global_sweeps_leave_tasks_of_a_removed_repo_alone() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "active/repo".to_string(),
    ));
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let router = router_with_healthy_review_agent();
    let active: HashSet<String> = ["active/repo".to_string()].into_iter().collect();

    let review_id = store
        .create_internal("removed/repo", "Needs review", "", "cron", "1", None)
        .await
        .unwrap();
    store
        .update_status(review_id, TaskStatus::NeedsReview)
        .await
        .unwrap();
    let blocked_id = store
        .create_internal("removed/repo", "Rebroadcast blocked", "", "cron", "2", None)
        .await
        .unwrap();
    store
        .update_status(blocked_id, TaskStatus::Blocked)
        .await
        .unwrap();
    store
        .set_block_reason(blocked_id, Some(REBROADCAST_BLOCK_REASON))
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z'")
        .execute(store.pool())
        .await
        .unwrap();

    refire_and_escalate_stale_needs_review_global(
        &task_manager,
        &store,
        &router,
        &dispatching,
        &active,
    )
    .await
    .unwrap();
    auto_recover_rebroadcast_blocked_tasks_global(&router, &task_manager, &store, &active)
        .await
        .unwrap();

    let review = store.get(review_id).await.unwrap();
    assert_eq!(review.status, TaskStatus::NeedsReview);
    assert_eq!(review.needs_review_refires, 0);
    let blocked = store.get(blocked_id).await.unwrap();
    assert_eq!(blocked.status, TaskStatus::Blocked);
    assert_eq!(
        blocked.block_reason.as_deref(),
        Some(REBROADCAST_BLOCK_REASON)
    );
}

// ── refire_and_escalate_stale_needs_review_global ──────────────────────

fn router_with_healthy_review_agent() -> Router {
    let mut config = RouterConfig::default();
    config
        .model_map
        .entry("review".to_string())
        .or_default()
        .insert("claude".to_string(), vec!["sonnet".to_string()]);
    let mut router = Router::new(config);
    router.available_agents = vec!["claude".to_string()];
    router
}

/// Regression test for #3469: a task stuck in NeedsReview for a repo that
/// is not the TaskManager's own repo must still be refired. The sweep
/// queries via `list_all_by_status_global` and writes through the store
/// directly, bypassing the repo-scoped TaskManager
#[serial(cooldown_state)]
#[tokio::test]
async fn global_refire_sweep_refires_stale_task_from_inactive_repo() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    // TaskManager scoped to the active repo — NOT the repo the task belongs to.
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "active/repo".to_string(),
    ));
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let router = router_with_healthy_review_agent();

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "inactive/repo",
            ext_id: "490",
            title: "Stale needs_review from inactive repo",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::NeedsReview)
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    refire_and_escalate_stale_needs_review_global(
        &task_manager,
        &store,
        &router,
        &dispatching,
        &sweep_repos(),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.needs_review_refires, 1,
        "cross-repo task must be refired"
    );
    assert_eq!(task.status, TaskStatus::NeedsReview);
}

/// After `MAX_NEEDS_REVIEW_REFIRE_ATTEMPTS` refires, a cross-repo task must
/// escalate to Blocked
#[serial(cooldown_state)]
#[tokio::test]
async fn global_refire_sweep_escalates_cross_repo_task_to_blocked() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "active/repo".to_string(),
    ));
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let router = router_with_healthy_review_agent();

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "inactive/repo",
            ext_id: "493",
            title: "Exhausted refires from inactive repo",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::NeedsReview)
        .await
        .unwrap();
    store
        .set_fields(id, &[("needs_review_refires", serde_json::json!(5))])
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    refire_and_escalate_stale_needs_review_global(
        &task_manager,
        &store,
        &router,
        &dispatching,
        &sweep_repos(),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(
        task.block_reason.as_deref(),
        Some("review agent rebroadcast escalated after repeated retries")
    );
}

/// When every review agent is cooled, the sweep must skip entirely.
/// Re-firing while all agents are cooled would waste the refire counter and
/// could escalate a task to Blocked because agents happened to be
/// temporarily unavailable
#[serial(cooldown_state)]
#[tokio::test]
async fn global_refire_sweep_skips_when_all_review_agents_cooled() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "active/repo".to_string(),
    ));
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    // RouterConfig::default() has no "review" model map entries, so the one
    // available agent has no healthy model for "review" — all cooled.
    let router = Router::new_for_test(RouterConfig::default(), vec!["claude".to_string()]);

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "inactive/repo",
            ext_id: "494",
            title: "Stale needs_review while agents cooled",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::NeedsReview)
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    refire_and_escalate_stale_needs_review_global(
        &task_manager,
        &store,
        &router,
        &dispatching,
        &sweep_repos(),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.needs_review_refires, 0,
        "counter must not be touched while cooled"
    );
    assert_eq!(task.status, TaskStatus::NeedsReview);
}

/// With no available review agent at all, the sweep must not count re-fires
/// or escalate to Blocked.
#[serial(cooldown_state)]
#[tokio::test]
async fn global_refire_sweep_skips_when_no_reviewer_available() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "active/repo".to_string(),
    ));
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let router = Router::new_for_test(RouterConfig::default(), vec![]);

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "some/repo",
            ext_id: "495",
            title: "needs_review with no reviewer",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::NeedsReview)
        .await
        .unwrap();
    store
        .set_fields(id, &[("needs_review_refires", serde_json::json!(5))])
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    refire_and_escalate_stale_needs_review_global(
        &task_manager,
        &store,
        &router,
        &dispatching,
        &sweep_repos(),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::NeedsReview);
    assert_eq!(task.needs_review_refires, 5);
}

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

/// A fresh InReview task (<1 min old) should NOT be reset — the review
/// agent may still be starting up.
#[serial(cooldown_state)]
#[tokio::test]
async fn stale_in_review_recovery_skips_fresh_tasks() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let router = Arc::new(RwLock::new(Router::new_for_test(
        RouterConfig::default(),
        vec!["claude".to_string()],
    )));
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let auto_merge_in_flight: Arc<DashSet<String>> = Arc::new(DashSet::new());

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "66",
            title: "Fresh review",
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
        .set_fields(id, &[("branch", serde_json::json!("feature/66"))])
        .await
        .unwrap();
    // Don't backdate — task is fresh (just created)

    Box::pin(sync_tick(
        &backend,
        &tmux,
        "owner/repo",
        &EngineConfig::default(),
        &router,
        &task_manager,
        &store,
        &dispatching,
        &auto_merge_in_flight,
    ))
    .await
    .unwrap();

    // Should stay InReview — too young to be considered orphaned
    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::InReview);
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

// ── FailureCategory::as_str ─────────────────────────────────────────

#[serial(cooldown_state)]
#[test]
fn failure_category_as_str() {
    // as_str() must return a stable, human-readable string used as the
    // reason key in auto_unblock tracking (stored in auto_unblock_last_reason).
    assert_eq!(FailureCategory::RateLimit.as_str(), "RateLimit");
    assert_eq!(FailureCategory::Timeout.as_str(), "Timeout");
    assert_eq!(
        FailureCategory::ModelUnavailable.as_str(),
        "ModelUnavailable"
    );
    assert_eq!(FailureCategory::MaxAttempts.as_str(), "MaxAttempts");
    assert_eq!(FailureCategory::Unknown.as_str(), "Unknown");
    assert_eq!(FailureCategory::SilentExit0.as_str(), "SilentExit0");
    assert_eq!(FailureCategory::ConnectionError.as_str(), "ConnectionError");
    assert_eq!(FailureCategory::CliFlagError.as_str(), "CliFlagError");
    assert_eq!(
        FailureCategory::AllAgentsExhausted.as_str(),
        "AllAgentsExhausted"
    );
    assert_eq!(FailureCategory::ParseError.as_str(), "ParseError");
    assert_eq!(FailureCategory::FalseFailure.as_str(), "FalseFailure");
    assert_eq!(FailureCategory::PushFailed.as_str(), "PushFailed");
    assert_eq!(FailureCategory::PrCreateFailed.as_str(), "PrCreateFailed");
}

// ── classify_failure: ModelUnavailable ──────────────────────────────

#[serial(cooldown_state)]
#[test]
fn classify_failure_model_unavailable_phrase_is_recoverable() {
    let category = classify_failure(
        "model unavailable (anthropic/claude-sonnet-4-6): Model not found: anthropic/claude-sonnet-4-6",
        "error",
    );
    assert_eq!(
        category,
        FailureCategory::ModelUnavailable,
        "\"model unavailable\" error must classify as ModelUnavailable"
    );
    assert!(
        category.is_recoverable(),
        "ModelUnavailable must be recoverable so auto-unblock can re-route"
    );
}

// ── classify_failure: CreditExhausted (billing cycle) ──────────────
#[serial(cooldown_state)]
#[test]
fn classify_failure_kimi_billing_cycle_is_credit_exhausted_not_rate_limit() {
    // Regression test for issue #3529: Kimi's billing-cycle exhaustion message
    // contains "usage limit", which also matches the generic rate-limit content
    // check. Credit exhaustion must be checked first so the failure is
    // categorized as CreditExhausted, not RateLimit, even when the stored
    // outcome is still "rate_limit" (e.g. from a task_run recorded before this
    // fix landed).
    let error = "kimi rate limit: Failed to authenticate. API Error: 403 You've reached \
                      your usage limit for this billing cycle. Your quota will be refreshed soon.";
    let category = classify_failure(error, "rate_limit");
    assert_eq!(
        category,
        FailureCategory::CreditExhausted,
        "billing-cycle exhaustion must classify as CreditExhausted, not RateLimit"
    );
    assert!(category.is_recoverable());
}

#[serial(cooldown_state)]
#[test]
fn classify_failure_billing_cycle_exhausted_outcome_is_credit_exhausted() {
    // New task_runs store the specific "billing_cycle_exhausted" outcome
    // (post-fix). classify_failure must recognize it directly too.
    let category = classify_failure("some billing message", "billing_cycle_exhausted");
    assert_eq!(category, FailureCategory::CreditExhausted);
}

#[serial(cooldown_state)]
#[test]
fn classify_failure_generic_rate_limit_is_still_rate_limit() {
    // Genuine rate limits (no billing-cycle/credit-exhaustion markers) must
    // remain RateLimit, not be swallowed by the reordered credit-exhaustion check.
    let category = classify_failure("429 Too Many Requests", "rate_limit");
    assert_eq!(category, FailureCategory::RateLimit);
}

// ── classify_failure: PrCreateFailed ──────────────────────────────
#[serial(cooldown_state)]
#[test]
fn classify_failure_pr_create_failed_specific_patterns() {
    // These should be classified as PrCreateFailed
    assert_eq!(
        classify_failure("create pr failed", ""),
        FailureCategory::PrCreateFailed
    );
    assert_eq!(
        classify_failure("failed to create pull request", ""),
        FailureCategory::PrCreateFailed
    );
    assert_eq!(
        classify_failure("pull request creation failed", ""),
        FailureCategory::PrCreateFailed
    );

    // These should NOT be classified as PrCreateFailed (they were before the fix)
    assert_ne!(
        classify_failure("network failed while fetching pull request status", ""),
        FailureCategory::PrCreateFailed
    );
    assert_ne!(
        classify_failure("connection failed, retrying pull request check", ""),
        FailureCategory::PrCreateFailed
    );
    assert_ne!(
        classify_failure("pull request created but failed to merge", ""),
        FailureCategory::PrCreateFailed
    );
}

#[serial(cooldown_state)]
#[test]
fn classify_failure_model_not_found_is_recoverable() {
    let category = classify_failure("Model not found: gpt-5-ultra", "error");
    assert_eq!(
        category,
        FailureCategory::ModelUnavailable,
        "\"model not found\" error must classify as ModelUnavailable"
    );
    assert!(category.is_recoverable());
}

#[serial(cooldown_state)]
#[test]
fn classify_failure_model_does_not_exist_is_recoverable() {
    let category = classify_failure("The model `claude-opus-99` does not exist", "error");
    assert_eq!(category, FailureCategory::ModelUnavailable);
    assert!(category.is_recoverable());
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

// ── auto_unblock: reason-reset (regression for #1227) ─────────────────

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

// ── classify_failure: sparse checkout (regression for substring false positive) ──

#[serial(cooldown_state)]
#[test]
fn classify_failure_sparse_checkout_not_parse_error() {
    // Regression test for #1204: "sparse" substring contains "parse",
    // so overly broad substring match would misclassify git sparse-checkout errors
    let category = classify_failure("error: cannot reset paths in a sparse checkout", "error");
    assert_ne!(
        category,
        FailureCategory::ParseError,
        "sparse checkout errors must NOT be classified as ParseError"
    );
    assert_eq!(
        category,
        FailureCategory::Unknown,
        "sparse checkout errors should be Unknown (not recoverable auto-unblock)"
    );
}

#[serial(cooldown_state)]
#[test]
fn classify_failure_sparse_index_not_parse_error() {
    // Another sparse-checkout variant that contains "parse"
    let category = classify_failure("failed due to sparse index configuration", "error");
    assert_ne!(category, FailureCategory::ParseError);
    assert_eq!(category, FailureCategory::Unknown);
}

// ── classify_failure: truncated (issue #3473) ───────────────────────

/// A review output truncated by the model's token budget (opencode
/// step_finish reason=length) must classify as recoverable, so a task
/// blocked after repeated truncations can be auto-unblocked instead of
/// requiring manual intervention.
#[serial(cooldown_state)]
#[test]
fn classify_failure_truncated_is_recoverable() {
    let category = classify_failure(
        "truncated: review output was cut off before completion \
             (output/reasoning token budget exceeded)",
        "truncated",
    );
    assert_eq!(
        category,
        FailureCategory::ParseError,
        "truncated outcome must classify as a recoverable parse-style failure"
    );
    assert!(
        category.is_recoverable(),
        "truncated failures must be recoverable so auto-unblock can retry"
    );
}

#[serial(cooldown_state)]
#[test]
fn dispatching_key_includes_repo_for_cross_project_isolation() {
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

    // Lock task 42 in repo A
    let key_a = "owner/repo-a/42".to_string();
    dispatching.insert(key_a.clone(), "42".to_string());

    // Same task ID in repo B should NOT be blocked
    let key_b = "owner/repo-b/42".to_string();
    assert!(dispatching.contains_key(&key_a));
    assert!(
        !dispatching.contains_key(&key_b),
        "same task ID in different repo should not be blocked"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn emit_degraded_agents_writes_metric_when_three_or_more_agents_degraded() {
    cooldown::reset_global_state().await;

    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    // Create a router and set deterministic available agents
    let mut config = RouterConfig::default();
    config
        .model_map
        .entry("medium".to_string())
        .or_default()
        .insert(
            "test-agent-3a".to_string(),
            vec!["test-model-a".to_string()],
        );
    config
        .model_map
        .entry("medium".to_string())
        .or_default()
        .insert(
            "test-agent-3b".to_string(),
            vec!["test-model-b".to_string()],
        );
    config
        .model_map
        .entry("medium".to_string())
        .or_default()
        .insert(
            "test-agent-3c".to_string(),
            vec!["test-model-c".to_string()],
        );
    config
        .model_map
        .entry("medium".to_string())
        .or_default()
        .insert(
            "test-agent-3d".to_string(),
            vec!["test-model-d".to_string()],
        );
    let mut router = Router::new(config);
    router.available_agents = vec![
        "test-agent-3a".to_string(),
        "test-agent-3b".to_string(),
        "test-agent-3c".to_string(),
        "test-agent-3d".to_string(),
    ];

    // Place three agents into cooldown
    set_agent_cooldown("test-agent-3a", 3600).await;
    set_agent_cooldown("test-agent-3b", 3600).await;
    set_agent_cooldown("test-agent-3c", 3600).await;

    // Call the helper and assert KV metrics written
    emit_degraded_agents_if_needed(&router.available_agents, &router.config, Some(&store)).await;

    // Count metric always written
    let count_val = store
        .kv_get("metrics:orch.agents_degraded.count")
        .await
        .unwrap();
    assert_eq!(count_val.as_deref(), Some("3"));

    // Alert metric set to "1" when threshold crossed
    let alert_val = store
        .kv_get("metrics:orch.agents_degraded.alert")
        .await
        .unwrap();
    assert_eq!(alert_val.as_deref(), Some("1"));
}

#[serial(cooldown_state)]
#[tokio::test]
async fn emit_degraded_agents_clears_alert_when_below_threshold() {
    cooldown::reset_global_state().await;

    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    let mut config = RouterConfig::default();
    config
        .model_map
        .entry("medium".to_string())
        .or_default()
        .insert(
            "test-agent-2a".to_string(),
            vec!["test-model-a".to_string()],
        );
    config
        .model_map
        .entry("medium".to_string())
        .or_default()
        .insert(
            "test-agent-2b".to_string(),
            vec!["test-model-b".to_string()],
        );
    let mut router = Router::new(config);
    router.available_agents = vec!["test-agent-2a".to_string(), "test-agent-2b".to_string()];

    // Place only two agents into cooldown — below the threshold of 3
    set_agent_cooldown("test-agent-2a", 3600).await;
    set_agent_cooldown("test-agent-2b", 3600).await;

    emit_degraded_agents_if_needed(&router.available_agents, &router.config, Some(&store)).await;

    // Count metric still written (always-emit)
    let count_val = store
        .kv_get("metrics:orch.agents_degraded.count")
        .await
        .unwrap();
    assert_eq!(count_val.as_deref(), Some("2"));

    // Alert metric cleared (set to "0") — no WARN should fire
    let alert_val = store
        .kv_get("metrics:orch.agents_degraded.alert")
        .await
        .unwrap();
    assert_eq!(alert_val.as_deref(), Some("0"));

    let stale_pool_alert = store
        .kv_get("metrics:orch.model_pool_stale_persistent.alert")
        .await
        .unwrap();
    assert_eq!(stale_pool_alert.as_deref(), Some("0"));
}

#[serial(cooldown_state)]
#[tokio::test]
async fn emit_degraded_agents_sets_stale_pool_alert_for_persistent_model_failures() {
    cooldown::reset_global_state().await;

    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    let mut config = RouterConfig::default();
    config
        .model_map
        .entry("medium".to_string())
        .or_default()
        .insert(
            "test-agent-stale".to_string(),
            vec!["model-a".to_string(), "model-b".to_string()],
        );
    let mut router = Router::new(config);
    router.available_agents = vec!["test-agent-stale".to_string()];

    // 50% cooled + persistent reason should trigger stale-pool alert.
    record_persistent_model_failure("test-agent-stale", "model-a").await;

    emit_degraded_agents_if_needed(&router.available_agents, &router.config, Some(&store)).await;

    let stale_pool_alert = store
        .kv_get("metrics:orch.model_pool_stale_persistent.alert")
        .await
        .unwrap();
    assert_eq!(stale_pool_alert.as_deref(), Some("1"));
}

// ── classify_comment tests ──────────────────────────────────────────

#[serial(cooldown_state)]
#[test]
fn classify_skip_already_processed() {
    let action = classify_comment(
        "@orch /retry",
        Some("https://api.github.com/repos/o/r/issues/1"),
        "@bot",
        true,
    );
    assert_eq!(action, CommentAction::Skip);
}

#[serial(cooldown_state)]
#[test]
fn classify_skip_irrelevant_comment() {
    let action = classify_comment("just a regular comment", None, "@bot", false);
    assert_eq!(action, CommentAction::Skip);
}

#[serial(cooldown_state)]
#[test]
fn classify_command_without_mention() {
    let action = classify_comment(
        "/retry",
        Some("https://api.github.com/repos/o/r/issues/42"),
        "@bot",
        false,
    );
    assert_eq!(
        action,
        CommentAction::ExecuteCommand {
            command: OwnerCommand::Retry,
            issue_num: "42".to_string(),
        }
    );
}

#[serial(cooldown_state)]
#[test]
fn classify_command_with_mention() {
    let action = classify_comment(
        "@orch /close",
        Some("https://api.github.com/repos/o/r/issues/7"),
        "@bot",
        false,
    );
    assert_eq!(
        action,
        CommentAction::ExecuteCommandForMention {
            command: OwnerCommand::Close,
            issue_num: "7".to_string(),
        }
    );
}

#[serial(cooldown_state)]
#[test]
fn classify_mention_without_command() {
    let action = classify_comment(
        "@orch can you look at this?",
        Some("https://api.github.com/repos/o/r/issues/99"),
        "@bot",
        false,
    );
    assert_eq!(
        action,
        CommentAction::CreateMentionTask {
            issue_num: Some("99".to_string()),
        }
    );
}

#[serial(cooldown_state)]
#[test]
fn classify_mention_without_issue_url() {
    let action = classify_comment("@orch help me", None, "@bot", false);
    assert_eq!(action, CommentAction::CreateMentionTask { issue_num: None });
}

#[serial(cooldown_state)]
#[test]
fn classify_command_no_mention_no_url_skips() {
    let action = classify_comment("/retry", None, "@bot", false);
    assert_eq!(action, CommentAction::Skip);
}

#[serial(cooldown_state)]
#[test]
fn classify_mention_via_current_user() {
    let action = classify_comment(
        "@bot please help",
        Some("https://api.github.com/repos/o/r/issues/5"),
        "@bot",
        false,
    );
    assert_eq!(
        action,
        CommentAction::CreateMentionTask {
            issue_num: Some("5".to_string()),
        }
    );
}

#[serial(cooldown_state)]
#[test]
fn classify_command_with_mention_no_url_creates_task() {
    // @mention + command but no issue URL → can't run command, create mention task
    let action = classify_comment("@orch /retry", None, "@bot", false);
    assert_eq!(action, CommentAction::CreateMentionTask { issue_num: None });
}

#[serial(cooldown_state)]
#[test]
fn mention_cursor_stops_advancing_after_gap() {
    let mut cursor = MentionCursor::default();
    cursor.advance("2026-01-01T00:00:00Z");
    cursor.block_on_gap();
    cursor.advance("2026-01-02T00:00:00Z");

    assert_eq!(
        cursor.into_last_safe_ts().as_deref(),
        Some("2026-01-01T00:00:00Z")
    );
}

#[serial(cooldown_state)]
#[test]
fn mention_cursor_tracks_max_timestamp_until_gap() {
    let mut cursor = MentionCursor::default();
    cursor.advance("2026-01-01T00:00:00Z");
    cursor.advance("2026-01-01T00:05:00Z");
    cursor.block_on_gap();
    cursor.advance("2026-01-01T00:10:00Z");

    assert_eq!(
        cursor.into_last_safe_ts().as_deref(),
        Some("2026-01-01T00:05:00Z")
    );
}

// ── scan_comments: acknowledge_mention failure does not advance cursor ────

/// Backend that tracks acknowledge_mention calls and can be configured to fail them.
struct AckTrackingBackend {
    ack_calls: Mutex<Vec<String>>,
    posted: Mutex<Vec<String>>,
    ack_failure: bool,
    inner: NoopBackend,
}

impl AckTrackingBackend {
    fn new(ack_failure: bool) -> Arc<Self> {
        Arc::new(Self {
            ack_calls: Mutex::new(Vec::new()),
            posted: Mutex::new(Vec::new()),
            ack_failure,
            inner: NoopBackend,
        })
    }

    fn fail_on_ack(&self) -> bool {
        self.ack_failure
    }
}

#[async_trait]
impl ExternalBackend for AckTrackingBackend {
    fn name(&self) -> &'static str {
        "ack-tracking"
    }

    async fn acknowledge_mention(&self, comment_id: &str) -> anyhow::Result<()> {
        self.ack_calls.lock().await.push(comment_id.to_string());
        if self.fail_on_ack() {
            // Return a transient error to simulate network glitch.
            anyhow::bail!("network error: temporary failure posting reaction");
        }
        Ok(())
    }

    // Override get_authenticated_user to ensure scan proceeds.
    async fn get_authenticated_user(&self) -> anyhow::Result<Option<String>> {
        Ok(Some("bot".into()))
    }

    // Override get_mentions to return empty (we use prefetched comments).
    async fn get_mentions(&self, _since: &str) -> anyhow::Result<Vec<Mention>> {
        Ok(vec![])
    }

    // Delegate everything else to NoopBackend.
    async fn create_task(&self, t: &str, b: &str, l: &[String]) -> anyhow::Result<ExternalId> {
        self.inner.create_task(t, b, l).await
    }
    async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask> {
        self.inner.get_task(id).await
    }
    async fn list_by_status(&self, s: Status) -> anyhow::Result<Vec<ExternalTask>> {
        self.inner.list_by_status(s).await
    }
    async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
        self.inner.list_routable().await
    }
    async fn post_comment(&self, id: &ExternalId, b: &str) -> anyhow::Result<()> {
        self.posted.lock().await.push(b.to_string());
        self.inner.post_comment(id, b).await
    }
    async fn set_labels(&self, id: &ExternalId, l: &[String]) -> anyhow::Result<()> {
        self.inner.set_labels(id, l).await
    }
    async fn remove_label(&self, id: &ExternalId, l: &str) -> anyhow::Result<()> {
        self.inner.remove_label(id, l).await
    }
    async fn get_sub_issues(&self, id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
        self.inner.get_sub_issues(id).await
    }
    async fn create_sub_task(
        &self,
        p: &ExternalId,
        t: &str,
        b: &str,
        l: &[String],
    ) -> anyhow::Result<ExternalId> {
        self.inner.create_sub_task(p, t, b, l).await
    }
    async fn ensure_status_label(&self, l: &str) -> anyhow::Result<()> {
        self.inner.ensure_status_label(l).await
    }
    async fn has_open_issue_with_title(&self, t: &str, l: &str) -> anyhow::Result<bool> {
        self.inner.has_open_issue_with_title(t, l).await
    }
    async fn health_check(&self) -> anyhow::Result<()> {
        self.inner.health_check().await
    }
    async fn is_pr_merged(&self, b: &str) -> anyhow::Result<bool> {
        self.inner.is_pr_merged(b).await
    }
    async fn update_status(&self, id: &ExternalId, s: Status) -> anyhow::Result<()> {
        self.inner.update_status(id, s).await
    }
}

/// When acknowledge_mention fails for CreateMentionTask, the cursor must NOT
/// advance and no mention task should be created. The next tick will retry.
#[serial(cooldown_state)]
#[tokio::test]
async fn create_mention_task_skips_on_ack_failure() {
    cooldown::reset_global_state().await;
    let backend: Arc<AckTrackingBackend> = AckTrackingBackend::new(true);
    let store: Arc<TaskStore> = Arc::new(TaskStore::open_memory().await.unwrap());

    // Set an initial cursor so we can verify it wasn't advanced.
    let initial_ts = "2026-01-01T00:00:00Z";
    store
        .kv_set("mentions_last_checked", initial_ts)
        .await
        .unwrap();

    let mention = Mention {
        id: "mention-abc".into(),
        body: "@bot can you help?".into(),
        author: "alice".into(),
        created_at: "2026-01-02T00:00:00Z".into(),
        issue_url: Some("https://api.github.com/repos/owner/repo/issues/42".into()),
    };

    let backend_trait: Arc<dyn ExternalBackend> = Arc::<AckTrackingBackend>::clone(&backend);
    let task_manager = Arc::new(TaskManager::new(Arc::clone(&backend_trait)));

    scan_comments(
        &backend_trait,
        Some(&store),
        "owner/repo",
        &task_manager,
        Some(&[mention]),
    )
    .await
    .unwrap();

    // acknowledge_mention was called with the mention ID.
    let calls = backend.ack_calls.lock().await;
    assert_eq!(
        &**calls,
        &["mention-abc"],
        "acknowledge_mention must be called"
    );

    // No mention task was created.
    let all = store.list_all("owner/repo").await.unwrap();
    assert!(
        all.is_empty(),
        "no task should be created when acknowledge_mention fails"
    );

    // Cursor was NOT advanced.
    let cursor = store.kv_get("mentions_last_checked").await.unwrap();
    assert_eq!(
        cursor.as_deref(),
        Some(initial_ts),
        "cursor must not advance when acknowledge_mention fails"
    );
}

/// When acknowledge_mention fails for ExecuteCommandForMention, handle_slash_command
/// must NOT be called and no mention task should be created.
#[serial(cooldown_state)]
#[tokio::test]
async fn execute_command_for_mention_skips_on_ack_failure() {
    cooldown::reset_global_state().await;
    let backend: Arc<AckTrackingBackend> = AckTrackingBackend::new(true);
    let store: Arc<TaskStore> = Arc::new(TaskStore::open_memory().await.unwrap());

    let initial_ts = "2026-01-01T00:00:00Z";
    store
        .kv_set("mentions_last_checked", initial_ts)
        .await
        .unwrap();

    // Mention with a slash command and issue URL → ExecuteCommandForMention.
    let mention = Mention {
        id: "mention-def".into(),
        body: "@bot /close".into(),
        author: "alice".into(),
        created_at: "2026-01-02T00:00:00Z".into(),
        issue_url: Some("https://api.github.com/repos/owner/repo/issues/42".into()),
    };

    let backend_trait: Arc<dyn ExternalBackend> = Arc::<AckTrackingBackend>::clone(&backend);
    let task_manager = Arc::new(TaskManager::new(Arc::clone(&backend_trait)));

    scan_comments(
        &backend_trait,
        Some(&store),
        "owner/repo",
        &task_manager,
        Some(&[mention]),
    )
    .await
    .unwrap();

    // acknowledge_mention was called.
    let calls = backend.ack_calls.lock().await;
    assert_eq!(&**calls, &["mention-def"]);

    // No mention task was created (handle_slash_command was not called).
    let all = store.list_all("owner/repo").await.unwrap();
    assert!(
        all.is_empty(),
        "no task should be created when acknowledge_mention fails for ExecuteCommandForMention"
    );

    // Cursor was NOT advanced.
    let cursor = store.kv_get("mentions_last_checked").await.unwrap();
    assert_eq!(
        cursor.as_deref(),
        Some(initial_ts),
        "cursor must not advance when acknowledge_mention fails for ExecuteCommandForMention"
    );
}

// ── slash commands retry after a transient GitHub error ──────────────────

/// GitHub stub whose `get_issue` fails once, then succeeds.
#[derive(Default)]
struct FlakyGh {
    get_issue_calls: AtomicUsize,
}

#[async_trait]
impl CommandGh for FlakyGh {
    async fn get_issue(&self, _repo: &str, number: &str) -> anyhow::Result<GitHubIssue> {
        if self.get_issue_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            anyhow::bail!("network error: temporary failure fetching issue");
        }
        Ok(GitHubIssue {
            number: number.parse().unwrap_or(0),
            id: 1,
            title: "t".into(),
            body: None,
            state: "open".into(),
            labels: Vec::new(),
            user: GitHubUser {
                login: "alice".into(),
            },
            created_at: String::new(),
            updated_at: String::new(),
            html_url: String::new(),
            node_id: None,
            pull_request: None,
            author_association: Some("OWNER".into()),
            state_reason: None,
        })
    }

    async fn is_collaborator(&self, _repo: &str, _username: &str) -> anyhow::Result<bool> {
        Ok(true)
    }

    async fn close_issue(&self, _repo: &str, _number: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A transient `get_issue` failure must leave the cursor before the comment so
/// the next scan retries it — and the command must then run exactly once.
#[serial(cooldown_state)]
#[tokio::test]
async fn slash_command_retries_after_transient_fetch_failure() {
    cooldown::reset_global_state().await;
    let backend: Arc<AckTrackingBackend> = AckTrackingBackend::new(false);
    let store: Arc<TaskStore> = Arc::new(TaskStore::open_memory().await.unwrap());
    let initial_ts = "2026-01-01T00:00:00Z";

    let mention = Mention {
        id: "mention-ghi".into(),
        body: "@bot /unblock".into(),
        author: "alice".into(),
        created_at: "2026-01-02T00:00:00Z".into(),
        issue_url: Some("https://api.github.com/repos/owner/repo/issues/42".into()),
    };

    let backend_trait: Arc<dyn ExternalBackend> = Arc::<AckTrackingBackend>::clone(&backend);
    let task_manager = Arc::new(TaskManager::new(Arc::clone(&backend_trait)));
    let gh = FlakyGh::default();
    let mut cursor = MentionCursor::default();

    // First scan: get_issue fails.
    let advanced_safely = handle_slash_command(
        &backend_trait,
        Some(&store),
        "owner/repo",
        &task_manager,
        &gh,
        &mention,
        &OwnerCommand::Unblock,
        &mut cursor,
    )
    .await
    .unwrap();

    assert!(
        !advanced_safely,
        "a transient fetch failure must stop cursor advancement"
    );
    assert!(
        cursor.last_safe_ts.is_none(),
        "cursor must stay before the comment so the next tick retries it"
    );
    assert!(
        store.list_all("owner/repo").await.unwrap().is_empty(),
        "no mention task on a transient failure"
    );
    assert!(
        backend.posted.lock().await.is_empty(),
        "nothing is posted to GitHub on a transient failure"
    );
    assert_eq!(
        filter_mentions_by_since(slice::from_ref(&mention), initial_ts).len(),
        1,
        "the comment is still in range for the next scan"
    );

    // Second scan: get_issue succeeds and the command runs.
    let mut cursor = MentionCursor::default();
    let advanced_safely = handle_slash_command(
        &backend_trait,
        Some(&store),
        "owner/repo",
        &task_manager,
        &gh,
        &mention,
        &OwnerCommand::Unblock,
        &mut cursor,
    )
    .await
    .unwrap();

    assert!(advanced_safely);
    assert_eq!(
        cursor.into_last_safe_ts().as_deref(),
        Some("2026-01-02T00:00:00Z"),
        "cursor advances once the command actually runs"
    );
    assert_eq!(
        backend.posted.lock().await.len(),
        1,
        "the command runs once, posting a single confirmation"
    );
    assert_eq!(
        store.list_all("owner/repo").await.unwrap().len(),
        1,
        "one sentinel mention task for the executed command"
    );
}

/// In the plain `ExecuteCommand` arm the GitHub client points at a dead port
/// under `cfg(test)`, so `get_issue` always fails — the cursor must stay put.
#[serial(cooldown_state)]
#[tokio::test]
async fn execute_command_keeps_cursor_on_fetch_failure() {
    cooldown::reset_global_state().await;
    let backend: Arc<AckTrackingBackend> = AckTrackingBackend::new(false);
    let store: Arc<TaskStore> = Arc::new(TaskStore::open_memory().await.unwrap());

    let initial_ts = "2026-01-01T00:00:00Z";
    store
        .kv_set("mentions_last_checked", initial_ts)
        .await
        .unwrap();

    // Slash command without a mention → ExecuteCommand.
    let comment = Mention {
        id: "comment-jkl".into(),
        body: "/unblock".into(),
        author: "alice".into(),
        created_at: "2026-01-02T00:00:00Z".into(),
        issue_url: Some("https://api.github.com/repos/owner/repo/issues/42".into()),
    };

    let backend_trait: Arc<dyn ExternalBackend> = Arc::<AckTrackingBackend>::clone(&backend);
    let task_manager = Arc::new(TaskManager::new(Arc::clone(&backend_trait)));

    scan_comments(
        &backend_trait,
        Some(&store),
        "owner/repo",
        &task_manager,
        Some(&[comment]),
    )
    .await
    .unwrap();

    assert!(
        backend.posted.lock().await.is_empty(),
        "nothing is posted when the issue fetch fails"
    );
    assert_eq!(
        store
            .kv_get("mentions_last_checked")
            .await
            .unwrap()
            .as_deref(),
        Some(initial_ts),
        "cursor must not advance past a command that never ran"
    );
}

// ── is_ci_failure_block ────────────────────────────────────────────────

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

// ── ci_failure_unblock_cooldown_elapsed ────────────────────────────────

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

// ── auto_unblock_ci_failure_blocked_tasks_global ───────────────────────

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

// ── auto_recover_rebroadcast_blocked_tasks_global ───────────────────────

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
