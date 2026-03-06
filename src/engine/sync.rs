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
use crate::sidecar::REPO_CONTEXT;
use crate::tmux::TmuxManager;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::RwLock;

use super::cleanup::{check_merged_prs, cleanup_done_worktrees};
use super::review::{review_and_merge, review_open_prs, ReviewDecision};
use super::EngineConfig;

/// Sync tick — runs every 45s.
///
/// Handles less-frequent operations:
/// - Cleanup finished worktrees
/// - Check for merged PRs → mark tasks done
/// - Scan for @mentions
pub(crate) async fn sync_tick(
    backend: &Arc<dyn ExternalBackend>,
    tmux: &Arc<TmuxManager>,
    repo: &str,
    db: &Arc<Db>,
    config: &EngineConfig,
    router: &Arc<RwLock<Router>>,
    task_manager: &Arc<TaskManager>,
) -> anyhow::Result<()> {
    tracing::debug!("sync tick");

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
    if let Err(e) = review_open_prs(backend, db, repo, config).await {
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
            // Transition to InReview — this IS the atomic guard against duplicates.
            // For internal tasks, task_manager routes to SQLite; for external to GitHub labels.
            if let Err(e) = task_manager
                .update_task_status(&task.id, Status::InReview)
                .await
            {
                tracing::warn!(task_id, err = %e, "failed to transition to InReview");
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
                let needs_reset = match review_and_merge(
                    &task_c, &backend_c, &tmux_c, &repo_s, &router_c,
                )
                .await
                {
                    Ok(ReviewDecision::Failed(reason)) => {
                        tracing::error!(
                            task_id = tid,
                            reason,
                            "review agent failed — resetting to NeedsReview for retry"
                        );
                        true
                    }
                    Err(e) => {
                        tracing::error!(
                            task_id = tid, error = %e,
                            "review_and_merge failed — resetting to NeedsReview for retry"
                        );
                        true
                    }
                    Ok(_) => false,
                };
                if needs_reset {
                    if tid.starts_with("internal:") {
                        // Internal tasks: update SQLite via task_manager.
                        let _ = task_manager_c
                            .update_task_status(&ExternalId(tid.clone()), Status::NeedsReview)
                            .await;
                    } else {
                        // External tasks: retry with backoff.
                        reset_to_needs_review_with_retry(&backend_c, &tid).await;
                    }
                }
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
            let review_task_id = format!("{}-review", task.id.0);
            let review_session = tmux.session_name(repo, &review_task_id);
            let has_session = tmux.session_exists(&review_session).await;
            if !has_session {
                tracing::warn!(
                    task_id = task.id.0,
                    session = %review_session,
                    "InReview task has no active review session — resetting to NeedsReview"
                );
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
    let mut existing_mentions = std::collections::HashSet::new();
    for status in &[
        TaskStatus::New,
        TaskStatus::InProgress,
        TaskStatus::Done,
        TaskStatus::Blocked,
        TaskStatus::Routed,
        TaskStatus::InReview,
        TaskStatus::NeedsReview,
    ] {
        let tasks = db.list_internal_tasks_by_status(*status).await?;
        for t in tasks {
            if t.source == "mention" {
                existing_mentions.insert(t.source_id.clone());
            }
        }
    }

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
}
