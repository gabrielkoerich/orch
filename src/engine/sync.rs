//! Sync tick — periodic operations that run less frequently than the core tick.
//!
//! The sync tick runs every ~45 seconds and handles:
//! - Worktree cleanup for done tasks
//! - Merged-PR detection
//! - @mention scanning and internal task creation
//! - PR review processing and follow-up dispatch
//! - Stale InReview detection
//! - Owner /slash command scanning
//! - Skill repository syncing

use crate::backends::{ExternalBackend, Status};
use crate::cmd::CommandErrorContext;
use crate::config;
use crate::engine::router::Router;
use crate::engine::tasks::TaskManager;
use crate::store;
use crate::store::review_session_expected;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use crate::tmux::TmuxManager;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::RwLock;

/// Read a KV value from the store.
async fn kv_get_prefer_store(store: &Option<&Arc<TaskStore>>, key: &str) -> Option<String> {
    if let Some(s) = store {
        if let Ok(v) = s.kv_get(key).await {
            return v;
        }
    }
    None
}

/// Write a KV value to the store.
async fn kv_set_prefer_store(store: &Option<&Arc<TaskStore>>, key: &str, value: &str) {
    if let Some(s) = store {
        if let Err(e) = s.kv_set(key, value).await {
            tracing::warn!(key, err = %e, "kv_set failed");
        }
    }
}

use super::cleanup::{check_merged_prs, cleanup_done_worktrees};
use super::review_poll::review_open_prs;
use super::EngineConfig;

/// Sync tick — runs every 45s.
///
/// Handles less-frequent operations:
/// - Cleanup finished worktrees
/// - Check for merged PRs → mark tasks done
/// - Scan for @mentions
#[allow(clippy::too_many_arguments)]
pub(crate) async fn sync_tick(
    backend: &Arc<dyn ExternalBackend>,
    tmux: &Arc<TmuxManager>,
    repo: &str,
    config: &EngineConfig,
    router: &Arc<RwLock<Router>>,
    task_manager: &Arc<TaskManager>,
    store: &Arc<crate::store::TaskStore>,
    dispatching: &Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
) -> anyhow::Result<()> {
    tracing::debug!("sync tick");

    // 0. Ingest all active external tasks into the unified store.
    //    This ensures the store has data for tasks created before dual-write was added.
    if let Err(e) = ingest_external_tasks(backend, repo, store).await {
        tracing::debug!(err = %e, "external task ingest failed");
    }

    // 1. Cleanup worktrees for done tasks (background — must not block routing/dispatch)
    {
        let backend = Arc::clone(backend);
        let repo = repo.to_string();
        let task_manager = Arc::clone(task_manager);
        let store = Arc::clone(store);
        tokio::spawn(async move {
            if let Err(e) = cleanup_done_worktrees(&backend, &repo, &task_manager, &store).await {
                tracing::warn!(err = %e, "worktree cleanup failed");
            }
        });
    }

    // 2. Check for merged PRs (in_review → done)
    if let Err(e) = check_merged_prs(backend, repo, store, task_manager).await {
        tracing::warn!(err = %e, "PR merge check failed");
    }

    // 3. Scan for @mentions
    if let Err(e) = scan_mentions(backend, Some(store), repo).await {
        tracing::warn!(err = %e, "mention scan failed");
    }

    // 4. Review open PRs (parse review comments, create follow-ups)
    if let Err(e) = review_open_prs(backend, repo, config, task_manager, store, dispatching).await {
        tracing::warn!(err = %e, "PR review failed");
    }

    // 5. Detect stale InReview tasks and recover them.
    //
    // NeedsReview → InReview triggering is handled exclusively by the event-driven
    // subscriber (`src/engine/subscribers/review.rs`). Having the sync tick do the
    // same thing caused a race: both paths could fire simultaneously, each spawning
    // a review agent, each incrementing the failure counter on a single real failure —
    // prematurely blocking tasks at half the expected retry budget (issue #857).
    let enable_review = config::get("workflow.enable_review_agent")
        .map(|v| v != "false")
        .unwrap_or(true);
    if enable_review {
        // Detect stale InReview tasks (review agent crashed, no active tmux session).
        // Read from the store (includes both external and internal tasks).
        let in_review_tasks = {
            let mut tasks = if store.has_external_tasks(repo).await {
                store
                    .list_external_by_status(repo, TaskStatus::InReview)
                    .await
                    .unwrap_or_default()
                    .iter()
                    .map(crate::engine::tasks::store_task_to_external)
                    .collect::<Vec<_>>()
            } else {
                let tasks = backend
                    .list_by_status(Status::InReview)
                    .await
                    .unwrap_or_default();
                tasks
            };
            if let Ok(internal_in_review) = task_manager
                .list_internal_by_status(TaskStatus::InReview)
                .await
            {
                tasks.extend(internal_in_review);
            }
            tasks
        };
        for task in in_review_tasks {
            // Skip tasks currently being processed by the main tick (dispatch + review flow).
            let dispatch_key = format!("{}/{}", repo, task.id.0);
            {
                let guard = dispatching.lock().unwrap_or_else(|e| e.into_inner());
                if guard.contains(&dispatch_key) {
                    tracing::debug!(
                        task_id = task.id.0,
                        "task locked by dispatch flow, skipping stale check"
                    );
                    continue;
                }
            }
            // Skip tasks that just transitioned to InReview — allow time for the
            // review agent to start its tmux session before treating it as stale.
            // A task is only considered stale if it has been in InReview for > 5 minutes.
            const MIN_STALE_MINUTES: i64 = 5;
            match chrono::DateTime::parse_from_rfc3339(&task.updated_at) {
                Ok(updated_at) => {
                    let age = chrono::Utc::now() - updated_at.with_timezone(&chrono::Utc);
                    if age.num_minutes() < MIN_STALE_MINUTES {
                        tracing::debug!(
                            task_id = task.id.0,
                            age_seconds = age.num_seconds(),
                            "InReview task is too young to be considered stale, skipping"
                        );
                        continue;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = task.id.0,
                        ts = %task.updated_at,
                        err = %e,
                        "invalid updated_at timestamp — treating task as potentially stale"
                    );
                }
            }

            if !review_session_expected(store, repo, &task.id.0).await {
                tracing::debug!(
                    task_id = task.id.0,
                    "InReview task is waiting on PR review, skipping stale review-session recovery"
                );
                continue;
            }

            let review_task_id = format!("{}-review", task.id.0);
            let review_session = tmux.session_name(repo, &review_task_id);
            let has_session = tmux.session_exists(&review_session).await;
            if !has_session {
                tracing::warn!(
                    task_id = task.id.0,
                    session = %review_session,
                    "InReview task has no active review session — resetting to NeedsReview"
                );
                // Reset the failure counter: stale-session recovery is an infrastructure
                // event (tmux crash, service restart) not a genuine agent parse failure.
                // Keeping the counter would unfairly consume the budget for the next cycle.
                store::store_set(
                    &Some(Arc::clone(store)),
                    repo,
                    &task.id.0,
                    &[("review_agent_failures", serde_json::json!(0))],
                )
                .await;
                if let Err(e) = task_manager
                    .update_task_status(&task.id, Status::NeedsReview)
                    .await
                {
                    tracing::error!(task_id = %task.id.0, err = %e, "failed to reset stale InReview task — task may be stuck in InReview indefinitely");
                } else {
                    store::set_review_session_expected(store, repo, &task.id.0, false).await;
                }
            }
        }
    }

    // 5b. Re-fire events for stale NeedsReview tasks.
    //
    // Two cases where the event-driven subscriber in `subscribers/review.rs` can miss
    // a NeedsReview task:
    // 1. All review slots are busy when the NeedsReview event fires — subscriber skips
    //    with `try_acquire_owned()`, task stays in NeedsReview with no future trigger.
    // 2. The broadcast receiver lagged (fell behind) — NeedsReview events were dropped.
    //
    // For both cases: re-publish the NeedsReview status after a short idle window so the
    // subscriber can pick it up when a slot is free. Using `update_task_status` (re-fire)
    // rather than spawning the review agent directly keeps this path stateless and avoids
    // re-introducing the double-trigger race fixed in issue #857 — the subscriber is still
    // the sole spawner and uses the InReview transition as its atomic guard.
    if enable_review {
        let needs_review_tasks = if store.has_external_tasks(repo).await {
            let mut tasks = store
                .list_external_by_status(repo, TaskStatus::NeedsReview)
                .await
                .unwrap_or_default()
                .iter()
                .map(crate::engine::tasks::store_task_to_external)
                .collect::<Vec<_>>();
            if let Ok(internal) = task_manager
                .list_internal_by_status(TaskStatus::NeedsReview)
                .await
            {
                tasks.extend(internal);
            }
            tasks
        } else {
            let mut tasks = backend
                .list_by_status(Status::NeedsReview)
                .await
                .unwrap_or_default();
            if let Ok(internal) = task_manager
                .list_internal_by_status(TaskStatus::NeedsReview)
                .await
            {
                tasks.extend(internal);
            }
            tasks
        };

        const MIN_STALE_NEEDS_REVIEW_MINUTES: i64 = 5;
        tracing::info!(
            count = needs_review_tasks.len(),
            "sync catch-up: checking stale NeedsReview tasks"
        );
        for task in needs_review_tasks {
            // Only retry tasks that have been in NeedsReview long enough that the
            // subscriber should have handled them by now. Fresh tasks (just transitioned)
            // are likely still in flight via the event bus. Unparseable timestamps are
            // treated as stale (fall through to retry).
            let age_minutes =
                if let Ok(updated_at) = chrono::DateTime::parse_from_rfc3339(&task.updated_at) {
                    let age = chrono::Utc::now() - updated_at.with_timezone(&chrono::Utc);
                    if age.num_minutes() < MIN_STALE_NEEDS_REVIEW_MINUTES {
                        continue;
                    }
                    Some(age.num_minutes())
                } else {
                    None
                };

            // Skip tasks actively being dispatched (subscriber is working on them).
            let dispatch_key = format!("{}/{}", repo, task.id.0);
            {
                let guard = dispatching.lock().unwrap_or_else(|e| e.into_inner());
                if guard.contains(&dispatch_key) {
                    continue;
                }
            }

            tracing::info!(
                task_id = task.id.0,
                age_minutes,
                "sync catch-up: re-firing NeedsReview event for stale task"
            );
            if let Err(e) = task_manager
                .update_task_status(&task.id, Status::NeedsReview)
                .await
            {
                tracing::warn!(
                    task_id = task.id.0,
                    err = %e,
                    "sync catch-up: failed to re-fire NeedsReview event"
                );
            }
        }
    }

    // 6. Scan for owner /slash commands in issue comments
    if let Err(e) =
        super::commands::scan_commands(backend, repo, &Some(Arc::clone(store)), task_manager).await
    {
        tracing::warn!(err = %e, "owner command scan failed");
    }

    // 7. Sync skill repositories
    match skills_sync().await {
        Ok(()) => {
            // Invalidate the LLM router's skills catalog cache so new/updated
            // skill files are picked up on the next routing call.
            router.read().await.invalidate_skills_catalog();
        }
        Err(e) => {
            tracing::warn!(err = %e, "skills sync failed");
        }
    }

    Ok(())
}

/// Scan for @mentions and create internal tasks.
///
/// Checks recent issue comments for @orchestrator mentions,
/// creates internal tasks, and acknowledges them.
async fn scan_mentions(
    backend: &Arc<dyn ExternalBackend>,
    store: Option<&Arc<crate::store::TaskStore>>,
    repo: &str,
) -> anyhow::Result<()> {
    // Get the current user (for mention detection)
    let current_user = match backend.get_authenticated_user().await {
        Ok(Some(u)) => format!("@{}", u),
        Ok(None) => {
            tracing::debug!("backend does not support user identity, skipping mentions");
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(err = %e, "failed to get current user, skipping mentions");
            return Ok(());
        }
    };

    // Use persisted cursor if available, otherwise fall back to 24h ago
    let fallback = chrono::Utc::now() - chrono::Duration::hours(24);
    let since_str = match kv_get_prefer_store(&store, "mentions_last_checked").await {
        Some(ts) => ts,
        None => fallback.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    };

    let mentions = match backend.get_mentions(&since_str).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(err = %e, "failed to get mentions");
            return Ok(());
        }
    };

    // Get existing mention tasks across ALL statuses to avoid duplicates.
    let existing_mentions: std::collections::HashSet<String> = if let Some(s) = store {
        s.list_all_internal(repo)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.source == "mention")
            .map(|t| t.source_id.clone())
            .collect()
    } else {
        std::collections::HashSet::new()
    };

    for mention in mentions {
        // Skip if already processed
        if existing_mentions.contains(&mention.id) {
            continue;
        }

        if !mention.body.contains(&current_user) && !mention.body.contains("@orchestrator") {
            continue;
        }

        // Create internal task for this mention
        let title = format!("Respond to mention by @{}", mention.author);
        let task_body = format!("Mention by @{}:\n\n{}", mention.author, mention.body);

        if let Some(s) = store {
            let task_id = s
                .create_internal(repo, &title, &task_body, "mention", &mention.id)
                .await?;
            tracing::info!(task_id, mention_id = %mention.id, "created mention task");
        }
    }

    // Persist cursor so the next sync tick only fetches newer comments
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    kv_set_prefer_store(&store, "mentions_last_checked", &now).await;

    Ok(())
}

/// Sync skill repositories from config.
///
/// Reads the `skills:` list from config and clones/pulls each repository
/// to `~/.orch/skills/{repo}/`. This keeps skill documentation up-to-date
/// for agents.
async fn skills_sync() -> anyhow::Result<()> {
    let skills = match crate::config::get_skills() {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(err = %e, "no skills configured");
            return Ok(());
        }
    };

    if skills.is_empty() {
        tracing::debug!("no skills configured, skipping sync");
        return Ok(());
    }

    let skills_base = crate::home::skills_dir()?;
    let git_timeout = std::time::Duration::from_secs(60);

    for skill in skills {
        // Validate repo format to prevent path traversal
        if skill.repo.contains("..") || skill.repo.matches('/').count() != 1 {
            tracing::warn!(repo = %skill.repo, "invalid skill repo format, expected 'owner/repo'");
            continue;
        }

        let repo_dir = skills_base.join(&skill.repo);
        let repo_url = format!("https://github.com/{}.git", skill.repo);

        if repo_dir.exists() {
            // Pull latest changes with timeout
            tracing::debug!(repo = %skill.repo, "pulling skill repo");
            let pull_result = tokio::time::timeout(
                git_timeout,
                Command::new("git")
                    .args(["pull", "--ff-only", "--prune"])
                    .current_dir(&repo_dir)
                    .output_with_context(),
            )
            .await;

            match pull_result {
                Ok(Ok(output)) if !output.status.success() => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::warn!(repo = %skill.repo, err = %stderr, "git pull failed");
                }
                Ok(Ok(_)) => {
                    tracing::debug!(repo = %skill.repo, "skill repo updated");
                }
                Ok(Err(e)) => {
                    tracing::warn!(repo = %skill.repo, err = %e, "git pull error");
                }
                Err(_) => {
                    tracing::warn!(repo = %skill.repo, "git pull timed out after 60s");
                }
            }
        } else {
            // Clone the repository (shallow for efficiency)
            tracing::debug!(repo = %skill.repo, "cloning skill repo");
            let parent = repo_dir
                .parent()
                .ok_or_else(|| anyhow::anyhow!("skill repo path has no parent directory"))?;
            std::fs::create_dir_all(parent)?;
            let repo_dir_str = repo_dir
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("skill repo path is not valid UTF-8"))?;

            let clone_result = tokio::time::timeout(
                git_timeout,
                Command::new("git")
                    .args(["clone", "--depth", "1", &repo_url, repo_dir_str])
                    .output_with_context(),
            )
            .await;

            match clone_result {
                Ok(Ok(output)) if !output.status.success() => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::warn!(repo = %skill.repo, err = %stderr, "git clone failed");
                    // Clean up partial clone to allow retry on next tick
                    let _ = std::fs::remove_dir_all(&repo_dir);
                }
                Ok(Ok(_)) => {
                    tracing::info!(repo = %skill.repo, "skill repo cloned");
                }
                Ok(Err(e)) => {
                    tracing::warn!(repo = %skill.repo, err = %e, "git clone error");
                    let _ = std::fs::remove_dir_all(&repo_dir);
                }
                Err(_) => {
                    tracing::warn!(repo = %skill.repo, "git clone timed out after 60s");
                    let _ = std::fs::remove_dir_all(&repo_dir);
                }
            }
        }
    }

    Ok(())
}

/// Ingest all active external tasks into the unified SQLite store.
///
/// Upserts each task so the store stays in sync with the backend.
/// Fetches all open issues (including unlabeled ones) so newly created
/// issues appear in the store immediately, not only after they get a
/// `status:*` label.  This is best-effort — individual task failures
/// are logged and skipped.
pub(crate) async fn ingest_external_tasks(
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    store: &Arc<crate::store::TaskStore>,
) -> anyhow::Result<()> {
    // Fetch all open issues in one call (includes unlabeled issues).
    // Also fetch routable tasks which catches unlabeled issues on backends
    // where list_all_tasks only returns labeled ones.
    let mut seen = std::collections::HashSet::new();
    let mut all_tasks: Vec<(crate::backends::ExternalTask, Option<Status>)> = Vec::new();

    // 1. Routable tasks (unlabeled + status:new) — these are the ones that
    //    were previously missed because the per-status loop skipped them.
    if let Ok(routable) = backend.list_routable().await {
        for task in routable {
            if seen.insert(task.id.0.clone()) {
                all_tasks.push((task, None));
            }
        }
    }

    // 2. All labeled active statuses — for status sync on first ingest.
    let active_statuses = [
        Status::New,
        Status::Routed,
        Status::InProgress,
        Status::NeedsReview,
        Status::InReview,
        Status::Blocked,
    ];
    for status in &active_statuses {
        let tasks = match backend.list_by_status(*status).await {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(?status, ?e, "ingest: failed to list tasks");
                continue;
            }
        };
        for task in tasks {
            if seen.insert(task.id.0.clone()) {
                all_tasks.push((task, Some(*status)));
            }
        }
    }

    // 3. Upsert into the store.
    for (task, status) in &all_tasks {
        match store.ensure_external_task(repo, task).await {
            Ok(store_id) => {
                // Only sync status from backend → store for NEW tasks (first ingest).
                // Once a task exists in the store, its status is authoritative —
                // re-ingestion must not overwrite store-first status changes
                // (e.g., store has Routed but GitHub still shows New labels).
                if let Some(status) = status {
                    if let Ok(existing) = store.get(store_id).await {
                        if existing.status == crate::store::TaskStatus::New {
                            let db_status = crate::engine::tasks::status_to_task_status(*status);
                            if db_status != crate::store::TaskStatus::New {
                                if let Err(e) = store.update_status(store_id, db_status).await {
                                    tracing::debug!(
                                        task_id = task.id.0,
                                        ?e,
                                        "ingest: status sync failed"
                                    );
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::debug!(task_id = task.id.0, ?e, "ingest: upsert failed");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{ExternalId, ExternalTask, Mention, Status};
    use async_trait::async_trait;
    use std::sync::Arc;

    // ── ingest_external_tasks tests ─────────────────────────────────────────

    /// Mock backend that returns configurable tasks per status.
    struct IngestMockBackend {
        /// Stored as (status_label, tasks) pairs since Status doesn't impl Hash.
        tasks: Vec<(String, Vec<ExternalTask>)>,
    }

    impl IngestMockBackend {
        fn with_tasks(tasks: Vec<(Status, ExternalTask)>) -> Arc<Self> {
            let mut grouped: Vec<(String, Vec<ExternalTask>)> = Vec::new();
            for (status, task) in tasks {
                let label = status.as_label().to_string();
                if let Some(entry) = grouped.iter_mut().find(|(l, _)| l == &label) {
                    entry.1.push(task);
                } else {
                    grouped.push((label, vec![task]));
                }
            }
            Arc::new(Self { tasks: grouped })
        }
    }

    #[async_trait]
    impl ExternalBackend for IngestMockBackend {
        fn name(&self) -> &str {
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
    }

    fn make_ext_task(id: &str, title: &str) -> ExternalTask {
        ExternalTask {
            id: ExternalId(id.to_string()),
            title: title.to_string(),
            body: "".to_string(),
            state: "open".to_string(),
            labels: vec![],
            author: "user".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: format!("https://github.com/owner/repo/issues/{id}"),
        }
    }

    #[tokio::test]
    async fn ingest_upserts_tasks_into_store() {
        use crate::store::TaskStore;

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
        assert_eq!(task1.status, crate::store::TaskStatus::New);

        let task2 = store
            .get_by_external_id("owner/repo", "2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(task2.title, "Second issue");
        assert_eq!(task2.status, crate::store::TaskStatus::InProgress);
    }

    #[tokio::test]
    async fn ingest_updates_existing_tasks() {
        use crate::store::TaskStore;

        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![(
            Status::Routed,
            make_ext_task("42", "Updated title"),
        )]);
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        // Pre-create the task
        store
            .create(&crate::store::NewTask {
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
        assert_eq!(task.status, crate::store::TaskStatus::Routed);
    }

    #[tokio::test]
    async fn ingest_handles_empty_backend() {
        use crate::store::TaskStore;

        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        // Should not error on empty backend
        let result = ingest_external_tasks(&backend, "owner/repo", &store).await;
        assert!(result.is_ok());

        let all = store.list_all("owner/repo").await.unwrap();
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn ingest_syncs_status_correctly_across_statuses() {
        use crate::store::TaskStore;

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

    #[tokio::test]
    async fn ingest_does_not_overwrite_store_authoritative_status() {
        use crate::store::TaskStore;

        // Backend reports task as New (GitHub labels haven't caught up yet)
        let backend: Arc<dyn ExternalBackend> =
            IngestMockBackend::with_tasks(vec![(Status::New, make_ext_task("99", "My task"))]);
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        // Pre-create the task and advance it to Routed in the store
        let id = store
            .create(&crate::store::NewTask {
                external_id: Some("99".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "My task".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::Routed)
            .await
            .unwrap();

        // Ingest should NOT overwrite Routed back to New
        ingest_external_tasks(&backend, "owner/repo", &store)
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(
            task.status,
            crate::store::TaskStatus::Routed,
            "ingest must not overwrite store-authoritative status"
        );
    }

    // ── kv_get_prefer_store / kv_set_prefer_store ────────────────────

    #[tokio::test]
    async fn kv_get_prefer_store_reads_from_store() {
        let store = Arc::new(crate::store::TaskStore::open_memory().await.unwrap());

        store.kv_set("k1", "store_val").await.unwrap();

        let opt = Some(&store);
        let val = kv_get_prefer_store(&opt, "k1").await;
        assert_eq!(val.as_deref(), Some("store_val"));
    }

    #[tokio::test]
    async fn kv_get_prefer_store_returns_none_without_store() {
        let opt: Option<&Arc<crate::store::TaskStore>> = None;
        let val = kv_get_prefer_store(&opt, "k2").await;
        assert_eq!(val, None);
    }

    #[tokio::test]
    async fn kv_set_prefer_store_writes_to_store() {
        let store = Arc::new(crate::store::TaskStore::open_memory().await.unwrap());

        let opt = Some(&store);
        kv_set_prefer_store(&opt, "k3", "val3").await;

        assert_eq!(store.kv_get("k3").await.unwrap().as_deref(), Some("val3"));
    }

    #[tokio::test]
    async fn kv_set_prefer_store_noop_without_store() {
        let opt: Option<&Arc<crate::store::TaskStore>> = None;
        // Should not panic
        kv_set_prefer_store(&opt, "k4", "val4").await;
    }

    // ── dispatching lock tests ──────────────────────────────────────────

    #[test]
    fn dispatching_set_blocks_duplicate_processing() {
        let dispatching: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

        let repo = "owner/repo";
        let task_id = "42";
        let dispatch_key = format!("{}/{}", repo, task_id);

        // Initially not in the set
        {
            let guard = dispatching.lock().unwrap();
            assert!(!guard.contains(&dispatch_key));
        }

        // Insert — simulates dispatch starting
        {
            let mut guard = dispatching.lock().unwrap();
            guard.insert(dispatch_key.clone());
        }

        // Now should be blocked
        {
            let guard = dispatching.lock().unwrap();
            assert!(
                guard.contains(&dispatch_key),
                "task should be locked while dispatching"
            );
        }

        // Remove — simulates review completion
        {
            let mut guard = dispatching.lock().unwrap();
            guard.remove(&dispatch_key);
        }

        // Now should be free again
        {
            let guard = dispatching.lock().unwrap();
            assert!(
                !guard.contains(&dispatch_key),
                "task should be unlocked after review completes"
            );
        }
    }

    #[test]
    fn dispatching_set_does_not_block_other_tasks() {
        let dispatching: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

        let repo = "owner/repo";

        // Lock task 42
        let key_42 = format!("{}/42", repo);
        {
            let mut guard = dispatching.lock().unwrap();
            guard.insert(key_42.clone());
        }

        // Task 43 should NOT be blocked
        let key_43 = format!("{}/43", repo);
        {
            let guard = dispatching.lock().unwrap();
            assert!(guard.contains(&key_42), "task 42 should be locked");
            assert!(!guard.contains(&key_43), "task 43 should not be locked");
        }
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
    #[tokio::test]
    async fn dispatch_key_held_during_review_agent_execution() {
        use tokio::sync::oneshot;

        let dispatching: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

        let dispatch_key = "owner/repo/42".to_string();

        // Step 1: insert before spawning — this is the invariant added by a6d8b9a.
        {
            let mut guard = dispatching.lock().unwrap();
            guard.insert(dispatch_key.clone());
        }

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
            let mut guard = dispatching_agent.lock().unwrap();
            guard.remove(&key_agent);
        });

        // Step 3: concurrent review_open_prs check — simulates step 4 of another
        // sync_tick invocation running while the review agent is still active.
        agent_started_rx.await.unwrap();
        {
            let guard = dispatching.lock().unwrap();
            assert!(
                guard.contains(&dispatch_key),
                "dispatch_key must be visible in the dispatching set while the review \
                 agent is running — without the a6d8b9a fix, review_open_prs would see \
                 the key absent and re-dispatch the task, silently dropping \
                 CHANGES_REQUESTED feedback"
            );
        }

        // Step 4: review agent completes.
        check_done_tx.send(()).unwrap();
        review_task.await.unwrap();

        // Step 5: key released — review_open_prs can now act on the task if needed.
        {
            let guard = dispatching.lock().unwrap();
            assert!(
                !guard.contains(&dispatch_key),
                "dispatch_key must be removed from the dispatching set after the \
                 review agent completes so subsequent sync ticks can process the task"
            );
        }
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
    #[tokio::test]
    async fn dispatch_guard_releases_key_on_panic() {
        use crate::engine::dispatch_guard::DispatchGuard;
        use std::collections::HashSet;

        let dispatching: Arc<std::sync::Mutex<HashSet<String>>> =
            Arc::new(std::sync::Mutex::new(HashSet::new()));
        let key = "owner/repo/42".to_string();

        // Insert key before spawn — mirrors the production invariant established
        // by the a6d8b9a fix (key must be visible before the spawn so concurrent
        // review_open_prs callers see it and skip the task).
        {
            let mut g = dispatching.lock().unwrap();
            g.insert(key.clone());
        }

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
        let g = dispatching.lock().unwrap();
        assert!(
            !g.contains(&key),
            "dispatch key must be removed from the dispatching set even when the \
             spawned review task panics — without DispatchGuard the key leaks and \
             the task gets stuck in a permanent review loop"
        );
    }

    #[tokio::test]
    async fn stale_in_review_recovery_skips_tasks_waiting_for_human_review() {
        let store = Arc::new(crate::store::TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let router = Arc::new(RwLock::new(crate::engine::router::Router::from_config()));
        let dispatching: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: "55",
                title: "Waiting for human review",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::InReview)
            .await
            .unwrap();
        store
            .set_fields(id, &[("branch", serde_json::json!("feature/55"))])
            .await
            .unwrap();
        sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
            .bind(id)
            .execute(store.pool())
            .await
            .unwrap();

        sync_tick(
            &backend,
            &tmux,
            "owner/repo",
            &EngineConfig::default(),
            &router,
            &task_manager,
            &store,
            &dispatching,
        )
        .await
        .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(task.status, crate::store::TaskStatus::InReview);
    }

    #[test]
    fn dispatching_key_includes_repo_for_cross_project_isolation() {
        let dispatching: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

        // Lock task 42 in repo A
        let key_a = "owner/repo-a/42".to_string();
        {
            let mut guard = dispatching.lock().unwrap();
            guard.insert(key_a.clone());
        }

        // Same task ID in repo B should NOT be blocked
        let key_b = "owner/repo-b/42".to_string();
        {
            let guard = dispatching.lock().unwrap();
            assert!(guard.contains(&key_a));
            assert!(
                !guard.contains(&key_b),
                "same task ID in different repo should not be blocked"
            );
        }
    }
}
