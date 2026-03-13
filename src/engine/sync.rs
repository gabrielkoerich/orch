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

use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::cmd::CommandErrorContext;
use crate::config;
use crate::db::{Db, TaskStatus};
use crate::engine::router::Router;
use crate::engine::tasks::TaskManager;
use crate::sidecar::{self, REPO_CONTEXT};
use crate::tmux::TmuxManager;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::sync::Semaphore;

use super::cleanup::{check_merged_prs, cleanup_done_worktrees};
use super::review::{review_and_merge, review_open_prs, ReviewDecision, MAX_REVIEW_AGENT_FAILURES};
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
    db: &Arc<Db>,
    config: &EngineConfig,
    semaphore: &Arc<Semaphore>,
    router: &Arc<RwLock<Router>>,
    task_manager: &Arc<TaskManager>,
    store: &Arc<crate::store::TaskStore>,
) -> anyhow::Result<()> {
    tracing::debug!("sync tick");

    // 0. Ingest all active external tasks into the unified store.
    //    This ensures the store has data for tasks created before dual-write was added.
    if let Err(e) = ingest_external_tasks(backend, repo, store).await {
        tracing::debug!(err = %e, "external task ingest failed");
    }

    // 1. Cleanup worktrees for done tasks
    if let Err(e) = cleanup_done_worktrees(backend, repo, task_manager).await {
        tracing::warn!(err = %e, "worktree cleanup failed");
    }

    // 2. Check for merged PRs (in_review → done)
    if let Err(e) = check_merged_prs(backend).await {
        tracing::warn!(err = %e, "PR merge check failed");
    }

    // 3. Scan for @mentions
    if let Err(e) = scan_mentions(backend, db).await {
        tracing::warn!(err = %e, "mention scan failed");
    }

    // 4. Review open PRs (parse review comments, create follow-ups)
    if let Err(e) = review_open_prs(backend, db, repo, config, task_manager).await {
        tracing::warn!(err = %e, "PR review failed");
    }

    // 5. Trigger review agent for needs_review tasks (catch-up for any missed in main tick)
    let enable_review = config::get("workflow.enable_review_agent")
        .map(|v| v != "false")
        .unwrap_or(true);
    if enable_review {
        // Collect all NeedsReview tasks: external + internal.
        let mut needs_review_tasks = backend
            .list_by_status(Status::NeedsReview)
            .await
            .unwrap_or_default();
        if let Ok(internal_needs_review) = task_manager
            .db_list_internal_by_status(TaskStatus::NeedsReview)
            .await
        {
            for t in internal_needs_review {
                needs_review_tasks.push(ExternalTask {
                    id: ExternalId(format!("internal:{}", t.id)),
                    title: t.title,
                    body: t.body,
                    state: "open".to_string(),
                    labels: vec!["status:needs_review".to_string()],
                    author: t.source,
                    created_at: t.created_at.to_rfc3339(),
                    updated_at: t.updated_at.to_rfc3339(),
                    url: String::new(),
                });
            }
        }

        for task in needs_review_tasks {
            let task_id = &task.id.0;
            tracing::info!(task_id, "triggering review agent for needs_review task");
            let permit = match semaphore.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    tracing::debug!("all parallel slots busy, skipping remaining review tasks");
                    break;
                }
            };
            // Transition to InReview — this IS the atomic guard against duplicates.
            // For internal tasks, task_manager routes to SQLite; for external to GitHub labels.
            if let Err(e) = task_manager
                .update_task_status(&task.id, Status::InReview)
                .await
            {
                tracing::warn!(task_id, err = %e, "failed to transition to InReview");
                drop(permit);
                continue;
            }
            let backend_c = backend.clone();
            let task_manager_c = task_manager.clone();
            let tmux_c = tmux.clone();
            let task_c = task.clone();
            let repo_s = repo.to_string();
            let router_c = router.clone();
            let repo_ctx = repo_s.clone();
            tokio::spawn(REPO_CONTEXT.scope(repo_ctx, async move {
                let tid = task_c.id.0.clone();
                enum ReviewOutcome {
                    Reset,
                    Block,
                    Ok,
                }
                let outcome = match review_and_merge(
                    &task_c,
                    &backend_c,
                    &tmux_c,
                    &repo_s,
                    &router_c,
                    &task_manager_c,
                )
                .await
                {
                    Ok(ReviewDecision::Blocked(reason)) => {
                        tracing::error!(
                            task_id = tid,
                            reason,
                            "review gate blocked after repeated failures — marking task blocked"
                        );
                        ReviewOutcome::Block
                    }
                    Ok(ReviewDecision::Failed(reason)) => {
                        let failures =
                            sidecar::get_u64(&tid, "review_agent_failures").saturating_add(1);
                        let _ = sidecar::set(&tid, &[format!("review_agent_failures={failures}")]);
                        if failures >= MAX_REVIEW_AGENT_FAILURES {
                            tracing::error!(
                                task_id = tid,
                                reason,
                                failures,
                                "review agent failed too many times — blocking task"
                            );
                            ReviewOutcome::Block
                        } else {
                            tracing::error!(
                                task_id = tid,
                                reason,
                                failures,
                                "review agent failed — resetting to NeedsReview for retry"
                            );
                            ReviewOutcome::Reset
                        }
                    }
                    Err(e) => {
                        let failures =
                            sidecar::get_u64(&tid, "review_agent_failures").saturating_add(1);
                        let _ = sidecar::set(&tid, &[format!("review_agent_failures={failures}")]);
                        if failures >= MAX_REVIEW_AGENT_FAILURES {
                            tracing::error!(
                                task_id = tid,
                                error = %e,
                                failures,
                                "review_and_merge failed too many times — blocking task"
                            );
                            ReviewOutcome::Block
                        } else {
                            tracing::error!(
                                task_id = tid,
                                error = %e,
                                failures,
                                "review_and_merge failed — resetting to NeedsReview for retry"
                            );
                            ReviewOutcome::Reset
                        }
                    }
                    Ok(_) => {
                        let _ = sidecar::set(
                            &tid,
                            &[
                                "review_agent_failures=0".to_string(),
                                "merge_conflict_retries=0".to_string(),
                                "pr_create_failures=0".to_string(),
                                "ci_merge_failures=0".to_string(),
                            ],
                        );
                        ReviewOutcome::Ok
                    }
                };
                match outcome {
                    ReviewOutcome::Reset => {
                        if tid.starts_with("internal:") {
                            let _ = task_manager_c
                                .update_task_status(&ExternalId(tid.clone()), Status::NeedsReview)
                                .await;
                        } else {
                            reset_to_needs_review_with_retry(&backend_c, &tid).await;
                        }
                    }
                    ReviewOutcome::Block => {
                        let _ = task_manager_c
                            .update_task_status(&ExternalId(tid.clone()), Status::Blocked)
                            .await;
                    }
                    ReviewOutcome::Ok => {}
                }

                drop(permit);
            }));
        }

        // Detect stale InReview tasks (review agent crashed, no active tmux session).
        // Check external tasks.
        let mut in_review_tasks = backend
            .list_by_status(Status::InReview)
            .await
            .unwrap_or_default();
        // Also include internal InReview tasks.
        if let Ok(internal_in_review) = task_manager
            .db_list_internal_by_status(TaskStatus::InReview)
            .await
        {
            for t in internal_in_review {
                in_review_tasks.push(ExternalTask {
                    id: ExternalId(format!("internal:{}", t.id)),
                    title: t.title,
                    body: t.body,
                    state: "open".to_string(),
                    labels: vec!["status:in_review".to_string()],
                    author: t.source,
                    created_at: t.created_at.to_rfc3339(),
                    updated_at: t.updated_at.to_rfc3339(),
                    url: String::new(),
                });
            }
        }
        for task in in_review_tasks {
            // Skip tasks that just transitioned to InReview — allow time for the
            // review agent to start its tmux session before treating it as stale.
            // A task is only considered stale if it has been in InReview for > 5 minutes.
            const MIN_STALE_MINUTES: i64 = 5;
            if let Ok(updated_at) = chrono::DateTime::parse_from_rfc3339(&task.updated_at) {
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
                if let Err(e) = sidecar::set(&task.id.0, &["review_agent_failures=0".to_string()]) {
                    tracing::warn!(task_id = %task.id.0, err = %e, "failed to reset review_agent_failures on stale-session recovery");
                }
                if let Err(e) = task_manager
                    .update_task_status(&task.id, Status::NeedsReview)
                    .await
                {
                    tracing::error!(task_id = %task.id.0, err = %e, "failed to reset stale InReview task — task may be stuck in InReview indefinitely");
                }
            }
        }
    }

    // 6. Scan for owner /slash commands in issue comments
    if let Err(e) = super::commands::scan_commands(backend, db, repo).await {
        tracing::warn!(err = %e, "owner command scan failed");
    }

    // 7. Sync skill repositories
    if let Err(e) = skills_sync().await {
        tracing::warn!(err = %e, "skills sync failed");
    }

    Ok(())
}

/// Reset a task to NeedsReview, retrying up to 3 times with exponential backoff.
///
/// If the reset fails on every attempt, an error is logged and the task will
/// remain stuck in InReview until the stale-detection sweep rescues it (≤45s).
async fn reset_to_needs_review_with_retry(backend: &Arc<dyn ExternalBackend>, task_id: &str) {
    let eid = ExternalId(task_id.to_string());
    for attempt in 1u32..=3 {
        match backend.update_status(&eid, Status::NeedsReview).await {
            Ok(_) => return,
            Err(e) if attempt < 3 => {
                tracing::warn!(
                    task_id, attempt, err = %e,
                    "failed to reset review task to NeedsReview, retrying"
                );
                tokio::time::sleep(std::time::Duration::from_secs(attempt as u64 * 2)).await;
            }
            Err(e) => {
                tracing::error!(
                    task_id, err = %e,
                    "all retries exhausted resetting to NeedsReview — task stuck in InReview until stale sweep"
                );
            }
        }
    }
}

/// Scan for @mentions and create internal tasks.
///
/// Checks recent issue comments for @orchestrator mentions,
/// creates internal tasks, and acknowledges them.
async fn scan_mentions(backend: &Arc<dyn ExternalBackend>, db: &Arc<Db>) -> anyhow::Result<()> {
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
    let since_str = match db.kv_get("mentions_last_checked").await {
        Ok(Some(ts)) => ts,
        _ => fallback.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    };

    let mentions = match backend.get_mentions(&since_str).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(err = %e, "failed to get mentions");
            return Ok(());
        }
    };

    // Get existing mention tasks across ALL statuses to avoid duplicates.
    // Only checking New status would miss tasks that progressed to InProgress/Done,
    // causing duplicate tasks on the next sync tick within the 24h window.
    let existing_mentions: std::collections::HashSet<String> = db
        .list_all_internal_tasks()
        .await?
        .into_iter()
        .filter(|t| t.source == "mention")
        .map(|t| t.source_id.clone())
        .collect();

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

        let task_id = db
            .create_internal_task(&title, &task_body, "mention", &mention.id)
            .await?;

        tracing::info!(task_id, mention_id = %mention.id, "created mention task");
    }

    // Persist cursor so the next sync tick only fetches newer comments
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    if let Err(e) = db.kv_set("mentions_last_checked", &now).await {
        tracing::warn!(err = %e, "failed to persist mentions cursor");
    }

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
/// Also syncs sidecar fields for each task so the store has the latest
/// routing, execution, and cost data. This is best-effort — individual
/// task failures are logged and skipped.
pub(crate) async fn ingest_external_tasks(
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    store: &Arc<crate::store::TaskStore>,
) -> anyhow::Result<()> {
    // Ingest tasks across all active statuses.
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

        for task in &tasks {
            match store.ensure_external_task(repo, task).await {
                Ok(store_id) => {
                    // Sync sidecar fields to store (best-effort)
                    store.sync_sidecar_to_store(store_id, &task.id.0).await;
                    // Sync status from backend label to store
                    let db_status = crate::engine::tasks::status_to_task_status(*status);
                    if let Err(e) = store.update_status(store_id, db_status).await {
                        tracing::debug!(task_id = task.id.0, ?e, "ingest: status sync failed");
                    }
                }
                Err(e) => {
                    tracing::debug!(task_id = task.id.0, ?e, "ingest: upsert failed");
                }
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
    use std::sync::{Arc, Mutex};

    /// Mock backend where `update_status` fails for the first `fail_first_n` calls,
    /// then succeeds. All other methods are stubs.
    struct CountingBackend {
        calls: Arc<Mutex<u32>>,
        fail_first_n: u32,
    }

    impl CountingBackend {
        fn new(fail_first_n: u32) -> Arc<Self> {
            Arc::new(Self {
                calls: Arc::new(Mutex::new(0)),
                fail_first_n,
            })
        }

        fn call_count(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl ExternalBackend for CountingBackend {
        fn name(&self) -> &str {
            "counting-mock"
        }

        async fn create_task(&self, _: &str, _: &str, _: &[String]) -> anyhow::Result<ExternalId> {
            Ok(ExternalId("1".into()))
        }

        async fn get_task(&self, _: &ExternalId) -> anyhow::Result<ExternalTask> {
            Ok(ExternalTask {
                id: ExternalId("1".into()),
                title: "test".into(),
                body: "".into(),
                state: "open".into(),
                labels: vec![],
                author: "user".into(),
                created_at: "2024-01-01T00:00:00Z".into(),
                updated_at: "2024-01-01T00:00:00Z".into(),
                url: "https://github.com/owner/repo/issues/1".into(),
            })
        }

        /// Override update_status so we control success/failure directly, bypassing
        /// the default get_task/remove_label/set_labels orchestration.
        async fn update_status(&self, _: &ExternalId, _: Status) -> anyhow::Result<()> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if *calls <= self.fail_first_n {
                anyhow::bail!("simulated failure on call {}", *calls)
            } else {
                Ok(())
            }
        }

        async fn list_by_status(&self, _: Status) -> anyhow::Result<Vec<ExternalTask>> {
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
    }

    #[tokio::test(start_paused = true)]
    async fn reset_succeeds_on_first_try() {
        let backend = CountingBackend::new(0);
        let backend_arc: Arc<dyn ExternalBackend> = backend.clone();
        reset_to_needs_review_with_retry(&backend_arc, "task-1").await;
        assert_eq!(
            backend.call_count(),
            1,
            "should call update_status exactly once"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reset_retries_on_transient_failures_and_succeeds() {
        // Fail first 2 calls, succeed on the 3rd
        let backend = CountingBackend::new(2);
        let backend_arc: Arc<dyn ExternalBackend> = backend.clone();
        reset_to_needs_review_with_retry(&backend_arc, "task-1").await;
        assert_eq!(backend.call_count(), 3, "should retry twice then succeed");
    }

    #[tokio::test(start_paused = true)]
    async fn reset_stops_after_three_attempts_when_exhausted() {
        // Always fail — function must not panic, just log error and stop
        let backend = CountingBackend::new(10);
        let backend_arc: Arc<dyn ExternalBackend> = backend.clone();
        reset_to_needs_review_with_retry(&backend_arc, "task-1").await;
        assert_eq!(
            backend.call_count(),
            3,
            "should attempt exactly 3 times then give up"
        );
    }

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
        assert_eq!(task1.status, crate::db::TaskStatus::New);

        let task2 = store
            .get_by_external_id("owner/repo", "2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(task2.title, "Second issue");
        assert_eq!(task2.status, crate::db::TaskStatus::InProgress);
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
        assert_eq!(task.status, crate::db::TaskStatus::Routed);
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
}
