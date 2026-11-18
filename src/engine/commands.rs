//! Owner slash commands — parse, validate, and execute /commands in issue comments.
//!
//! Provides command parsing (`parse_command`), validation + execution
//! (`validate_and_run_command`), and the individual command implementations.
//! The scanning loop lives in `sync.rs` (`scan_comments`).

use crate::backends::{ExternalBackend, ExternalId, Status};
use crate::engine;
use crate::engine::tasks::TaskManager;
use crate::github::http::GhHttp;
use crate::github::types::GitHubIssue;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use async_trait::async_trait;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::sync::Arc;

/// Minimal store interface required by command execution.
///
/// Exists so tests can inject a mock that simulates DB failures without
/// coupling the test harness to a real SQLite pool.
#[async_trait]
pub trait CommandStoreOps: Send + Sync {
    async fn resolve_task_id(&self, repo: &str, task_id: &str) -> anyhow::Result<Option<i64>>;

    async fn set_block_reason(&self, id: i64, reason: Option<&str>) -> anyhow::Result<()>;

    /// Reset all per-attempt counters for a task. Fire-and-forget — failures are
    /// logged but not propagated (same semantics as `store::store_reset_counters`).
    async fn reset_counters(&self, id: i64) -> anyhow::Result<()>;

    /// Current status of a task, or `None` when the store has no such task.
    async fn status(&self, repo: &str, task_id: &str) -> anyhow::Result<Option<TaskStatus>>;
}

#[async_trait]
impl CommandStoreOps for TaskStore {
    async fn resolve_task_id(&self, repo: &str, task_id: &str) -> anyhow::Result<Option<i64>> {
        self.resolve_task_id(repo, task_id).await
    }

    async fn set_block_reason(&self, id: i64, reason: Option<&str>) -> anyhow::Result<()> {
        self.set_block_reason(id, reason).await
    }

    async fn reset_counters(&self, id: i64) -> anyhow::Result<()> {
        self.reset_counters(id).await
    }

    async fn status(&self, repo: &str, task_id: &str) -> anyhow::Result<Option<TaskStatus>> {
        let Some(id) = self.resolve_task_id(repo, task_id).await? else {
            return Ok(None);
        };
        Ok(Some(self.get(id).await?.status))
    }
}

/// Minimal GitHub interface required by command validation and execution.
///
/// Exists so tests can inject a mock that simulates transient API failures
/// without reaching the network.
#[async_trait]
pub trait CommandGh: Send + Sync {
    async fn get_issue(&self, repo: &str, number: &str) -> anyhow::Result<GitHubIssue>;

    async fn is_collaborator(&self, repo: &str, username: &str) -> anyhow::Result<bool>;

    async fn close_issue(&self, repo: &str, number: &str) -> anyhow::Result<()>;
}

#[async_trait]
impl CommandGh for GhHttp {
    async fn get_issue(&self, repo: &str, number: &str) -> anyhow::Result<GitHubIssue> {
        GhHttp::get_issue(self, repo, number).await
    }

    async fn is_collaborator(&self, repo: &str, username: &str) -> anyhow::Result<bool> {
        GhHttp::is_collaborator(self, repo, username).await
    }

    async fn close_issue(&self, repo: &str, number: &str) -> anyhow::Result<()> {
        GhHttp::close_issue(self, repo, number).await
    }
}

/// Parsed owner command from an issue comment.
#[derive(Debug, Clone, PartialEq)]
pub enum OwnerCommand {
    /// Reset to status:new, re-dispatch.
    Retry,
    /// Clear agent, re-route (optionally force agent).
    Reroute(Option<String>),
    /// Mark status:done, close issue.
    Close,
    /// Mark status:blocked with optional reason.
    Block(Option<String>),
    /// Mark status:new, re-dispatch.
    Unblock,
    /// Trigger review agent on current PR.
    Review,
}

impl Display for OwnerCommand {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Retry => write!(f, "/retry"),
            Self::Reroute(Some(a)) => write!(f, "/reroute {a}"),
            Self::Reroute(None) => write!(f, "/reroute"),
            Self::Close => write!(f, "/close"),
            Self::Block(Some(r)) => write!(f, "/block {r}"),
            Self::Block(None) => write!(f, "/block"),
            Self::Unblock => write!(f, "/unblock"),
            Self::Review => write!(f, "/review"),
        }
    }
}

/// Parse a slash command from a comment body.
///
/// Scans each line for a `/command` at the start, optionally prefixed by an
/// `@orch` or `@orchestrator` mention on the same line. Returns the first
/// valid command found. Unknown `/something` lines are skipped. Lines inside
/// markdown fenced code blocks (``` or ~~~) are ignored to prevent accidental
/// command execution from code examples. Per CommonMark, a backtick fence
/// closes only a backtick fence and a tilde fence closes only a tilde fence —
/// mismatched closers are ignored.
pub fn parse_command(body: &str) -> Option<OwnerCommand> {
    // Track the opening fence (character, length) so mismatched or shorter
    // closers are ignored. Per CommonMark: a backtick fence closes only a
    // backtick fence, a tilde fence closes only a tilde fence, and the closing
    // fence must be at least as long as the opening fence.
    let mut fence_state: Option<(char, usize)> = None;
    for line in body.lines() {
        let trimmed = line.trim();
        let fence_ch = if trimmed.starts_with("```") {
            Some('`')
        } else if trimmed.starts_with("~~~") {
            Some('~')
        } else {
            None
        };
        if let Some(ch) = fence_ch {
            let run_len = trimmed.chars().take_while(|&c| c == ch).count();
            match fence_state {
                None => fence_state = Some((ch, run_len)),
                Some((c, len)) if c == ch && run_len >= len => fence_state = None,
                Some(_) => {} // mismatched closer or shorter — ignore
            }
            continue;
        }
        if fence_state.is_some() {
            continue;
        }
        let command_text = trimmed
            .strip_prefix("@orchestrator")
            .or_else(|| trimmed.strip_prefix("@orch"))
            .map_or(trimmed, str::trim_start);

        if !command_text.starts_with('/') {
            continue;
        }
        let parts: Vec<&str> = command_text.splitn(2, char::is_whitespace).collect();
        let cmd = parts[0];
        let args = parts
            .get(1)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(String::from);

        match cmd {
            "/retry" => return Some(OwnerCommand::Retry),
            "/reroute" => return Some(OwnerCommand::Reroute(args)),
            "/close" => return Some(OwnerCommand::Close),
            "/block" => return Some(OwnerCommand::Block(args)),
            "/unblock" => return Some(OwnerCommand::Unblock),
            "/review" => return Some(OwnerCommand::Review),
            _ => {}
        }
    }
    None
}

/// Reset all attempt/failure counters for a task. Fire-and-forget — errors are
/// logged as warnings but not returned so `/retry` and `/reroute` still succeed.
async fn cmd_reset_counters(store: Option<&Arc<dyn CommandStoreOps>>, repo: &str, task_id: &str) {
    let Some(s) = store else { return };
    match s.resolve_task_id(repo, task_id).await {
        Ok(Some(id)) => {
            if let Err(e) = s.reset_counters(id).await {
                tracing::warn!(task_id, err = %e, "store reset_counters failed");
            }
        }
        Ok(None) => {} // task not in store — no-op is expected
        Err(e) => {
            tracing::warn!(task_id, err = %e, "resolve_task_id failed in store write helper");
        }
    }
}

/// Refuse a command that would move a done task back into the pipeline.
///
/// Done is terminal: a revived task re-runs the body it was created with, which
/// may be months stale and duplicate work that already has its own task (#3780)
async fn refuse_if_done(
    store: Option<&Arc<dyn CommandStoreOps>>,
    repo: &str,
    task_id: &str,
    command: &OwnerCommand,
) -> anyhow::Result<()> {
    let Some(s) = store else { return Ok(()) };
    if s.status(repo, task_id).await? != Some(TaskStatus::Done) {
        return Ok(());
    }
    anyhow::bail!(
        "`{command}` — task is done and done is terminal; open a new issue for follow-up work"
    )
}

async fn clear_agent_reset_to_new(
    backend: &Arc<dyn ExternalBackend>,
    store: Option<&Arc<dyn CommandStoreOps>>,
    repo: &str,
    task_id: &ExternalId,
    task_manager: &Arc<TaskManager>,
    forced_agent: Option<&str>,
) -> anyhow::Result<()> {
    let task = backend.get_task(task_id).await?;
    for label in &task.labels {
        if label.starts_with("agent:") {
            backend.remove_label(task_id, label).await?;
        }
    }
    cmd_reset_counters(store, repo, &task_id.0).await;
    if let Some(agent_name) = forced_agent {
        let label = format!("agent:{agent_name}");
        backend.set_labels(task_id, &[label]).await?;
    }
    task_manager
        .update_task_status(task_id, Status::New)
        .await?;
    Ok(())
}

async fn write_block_reason(
    store: Option<&Arc<dyn CommandStoreOps>>,
    repo: &str,
    task_id: &str,
    reason: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(store) = store {
        let store_id = store.resolve_task_id(repo, task_id).await?;
        if let Some(store_id) = store_id {
            store.set_block_reason(store_id, reason).await?;
        }
    }
    Ok(())
}

/// Execute a single owner command against a task.
pub async fn execute_command(
    backend: &Arc<dyn ExternalBackend>,
    gh: &dyn CommandGh,
    repo: &str,
    task_id: &ExternalId,
    command: &OwnerCommand,
    store: &Option<Arc<dyn CommandStoreOps>>,
    task_manager: &Arc<TaskManager>,
) -> anyhow::Result<String> {
    match command {
        OwnerCommand::Retry => {
            refuse_if_done(store.as_ref(), repo, &task_id.0, command).await?;
            clear_agent_reset_to_new(backend, store.as_ref(), repo, task_id, task_manager, None)
                .await?;
            Ok("`/retry` — reset attempts, cleared agent, reset to `status:new`".to_string())
        }

        OwnerCommand::Reroute(agent) => {
            refuse_if_done(store.as_ref(), repo, &task_id.0, command).await?;
            clear_agent_reset_to_new(
                backend,
                store.as_ref(),
                repo,
                task_id,
                task_manager,
                agent.as_deref(),
            )
            .await?;
            match agent {
                Some(a) => Ok(format!(
                    "`/reroute {a}` — cleared agent, reset attempts, forced `agent:{a}`, reset to `status:new`"
                )),
                None => {
                    Ok("`/reroute` — cleared agent, reset attempts, reset to `status:new`, will re-route".into())
                }
            }
        }

        OwnerCommand::Close => {
            task_manager
                .update_task_status(task_id, Status::Done)
                .await?;
            gh.close_issue(repo, &task_id.0).await?;
            Ok("`/close` — marked `status:done` and closed issue".to_string())
        }

        OwnerCommand::Block(reason) => {
            write_block_reason(store.as_ref(), repo, &task_id.0, reason.as_deref()).await?;
            task_manager
                .update_task_status(task_id, Status::Blocked)
                .await?;
            match reason {
                Some(r) => Ok(format!("`/block` — marked `status:blocked`: {r}")),
                None => Ok("`/block` — marked `status:blocked`".to_string()),
            }
        }

        OwnerCommand::Unblock => {
            refuse_if_done(store.as_ref(), repo, &task_id.0, command).await?;
            write_block_reason(store.as_ref(), repo, &task_id.0, None).await?;
            task_manager
                .update_task_status(task_id, Status::New)
                .await?;
            Ok("`/unblock` — marked `status:new`, will re-dispatch".to_string())
        }

        OwnerCommand::Review => {
            task_manager
                .update_task_status(task_id, Status::NeedsReview)
                .await?;
            Ok(
                "`/review` — set `status:needs_review`, review agent will pick up on next tick"
                    .to_string(),
            )
        }
    }
}

/// Result of attempting to validate and run a slash command.
///
/// Variants that successfully fetched issue data include `is_pr` so callers
/// don't need a second `get_issue` call.
#[derive(Debug)]
pub enum CommandOutcome {
    /// Command executed successfully or failed — either way, it was handled.
    Executed { is_pr: bool },
    /// Issue/PR is not open — command was skipped.
    NotOpen { is_pr: bool },
    /// Author is not a collaborator — command was skipped.
    NotCollaborator { is_pr: bool },
    /// Failed to fetch issue state — should retry.
    FetchFailed,
    /// Failed to check collaborator status — should retry.
    CollaboratorCheckFailed,
}

/// Validate a slash command (issue open, author is collaborator) and execute it.
///
/// Shared by both `scan_commands` (issue comments) and `scan_mentions` (mention
/// handling). Posts a confirmation or error comment on the issue/PR.
#[expect(
    clippy::too_many_arguments,
    reason = "two scan paths pass their own context to one shared validator"
)]
pub async fn validate_and_run_command(
    backend: &Arc<dyn ExternalBackend>,
    gh: &dyn CommandGh,
    repo: &str,
    issue_number: &str,
    command: &OwnerCommand,
    author: &str,
    store: &Option<Arc<dyn CommandStoreOps>>,
    task_manager: &Arc<TaskManager>,
) -> CommandOutcome {
    // Check issue state
    let issue_data = match gh.get_issue(repo, issue_number).await {
        Ok(data) => data,
        Err(e) => {
            tracing::warn!(issue = %issue_number, err = %e, "failed to check issue state");
            return CommandOutcome::FetchFailed;
        }
    };

    let is_pr = issue_data.pull_request.is_some();

    if issue_data.state != "open" {
        tracing::debug!(issue = %issue_number, state = %issue_data.state, command = %command, "ignoring slash command on non-open issue");
        return CommandOutcome::NotOpen { is_pr };
    }

    // Check collaborator status
    match gh.is_collaborator(repo, author).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::info!(author, command = %command, issue = %issue_number, "ignoring slash command from non-collaborator");
            return CommandOutcome::NotCollaborator { is_pr };
        }
        Err(e) => {
            tracing::warn!(author, err = %e, "failed to check collaborator status");
            return CommandOutcome::CollaboratorCheckFailed;
        }
    }

    // Execute and post result
    let task_id = ExternalId(issue_number.to_string());
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    match execute_command(backend, gh, repo, &task_id, command, store, task_manager).await {
        Ok(msg) => {
            tracing::info!(issue = %issue_number, author, command = %command, "executed command");
            let confirmation = format!(
                "[{now}] {msg} — executed by @{author}{}",
                engine::orch_footer()
            );
            if let Err(e) = backend.post_comment(&task_id, &confirmation).await {
                tracing::warn!(issue = %issue_number, err = %e, "failed to post confirmation");
            }
        }
        Err(e) => {
            tracing::warn!(issue = %issue_number, command = %command, err = %e, "failed to execute command");
            let error_msg = format!(
                "[{now}] Failed to execute `{command}`: {e}{}",
                engine::orch_footer()
            );
            if let Err(e2) = backend.post_comment(&task_id, &error_msg).await {
                tracing::warn!(issue = %issue_number, err = %e2, "failed to post error comment");
            }
        }
    }

    CommandOutcome::Executed { is_pr }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Mention, Status};
    use crate::store::{NewTask, TaskStore};
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::Mutex;

    struct MockBackend {
        task: ExternalTask,
        fail_remove_label: bool,
        removed_labels: Mutex<Vec<String>>,
        set_label_calls: Mutex<Vec<Vec<String>>>,
    }

    impl MockBackend {
        fn new(id: &str) -> Self {
            Self {
                task: ExternalTask {
                    id: ExternalId(id.to_string()),
                    title: "Task".to_string(),
                    body: String::new(),
                    state: "open".to_string(),
                    labels: vec![],
                    author: "bot".to_string(),
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                    updated_at: "2026-01-01T00:00:00Z".to_string(),
                    url: String::new(),
                },
                fail_remove_label: false,
                removed_labels: Mutex::new(Vec::new()),
                set_label_calls: Mutex::new(Vec::new()),
            }
        }

        fn with_labels(id: &str, labels: Vec<String>) -> Self {
            let mut b = Self::new(id);
            b.task.labels = labels;
            b
        }

        fn failing_remove_label(id: &str, labels: Vec<String>) -> Self {
            let mut b = Self::with_labels(id, labels);
            b.fail_remove_label = true;
            b
        }
    }

    #[async_trait]
    impl ExternalBackend for MockBackend {
        fn name(&self) -> &'static str {
            "mock"
        }

        async fn create_task(
            &self,
            _title: &str,
            _body: &str,
            _labels: &[String],
        ) -> anyhow::Result<ExternalId> {
            Ok(self.task.id.clone())
        }

        async fn get_task(&self, _id: &ExternalId) -> anyhow::Result<ExternalTask> {
            Ok(self.task.clone())
        }

        async fn list_by_status(&self, _status: Status) -> anyhow::Result<Vec<ExternalTask>> {
            Ok(vec![])
        }

        async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
            Ok(vec![])
        }

        async fn post_comment(&self, _id: &ExternalId, _body: &str) -> anyhow::Result<()> {
            Ok(())
        }

        async fn set_labels(&self, _id: &ExternalId, labels: &[String]) -> anyhow::Result<()> {
            self.set_label_calls.lock().unwrap().push(labels.to_vec());
            Ok(())
        }

        async fn remove_label(&self, _id: &ExternalId, label: &str) -> anyhow::Result<()> {
            if self.fail_remove_label {
                anyhow::bail!("simulated remove_label failure for {label}");
            }
            self.removed_labels.lock().unwrap().push(label.to_string());
            Ok(())
        }

        async fn get_sub_issues(&self, _id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
            Ok(vec![])
        }

        async fn create_sub_task(
            &self,
            _parent: &ExternalId,
            _title: &str,
            _body: &str,
            _labels: &[String],
        ) -> anyhow::Result<ExternalId> {
            Ok(ExternalId("child".to_string()))
        }

        async fn ensure_status_label(&self, _label: &str) -> anyhow::Result<()> {
            Ok(())
        }

        async fn has_open_issue_with_title(
            &self,
            _title: &str,
            _label: &str,
        ) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn health_check(&self) -> anyhow::Result<()> {
            Ok(())
        }

        async fn is_pr_merged(&self, _branch: &str) -> anyhow::Result<bool> {
            Ok(false)
        }

        async fn get_authenticated_user(&self) -> anyhow::Result<Option<String>> {
            Ok(Some("testbot".to_string()))
        }

        async fn get_mentions(&self, _since: &str) -> anyhow::Result<Vec<Mention>> {
            Ok(vec![])
        }

        async fn update_status(&self, _id: &ExternalId, _status: Status) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// Mock store whose `resolve_task_id` always returns an error.
    struct FailingStore;

    #[async_trait]
    impl CommandStoreOps for FailingStore {
        async fn resolve_task_id(
            &self,
            _repo: &str,
            _task_id: &str,
        ) -> anyhow::Result<Option<i64>> {
            anyhow::bail!("simulated resolve_task_id DB failure")
        }

        async fn set_block_reason(&self, _id: i64, _reason: Option<&str>) -> anyhow::Result<()> {
            anyhow::bail!("simulated set_block_reason DB failure")
        }

        async fn reset_counters(&self, _id: i64) -> anyhow::Result<()> {
            Ok(())
        }

        async fn status(&self, _repo: &str, _task_id: &str) -> anyhow::Result<Option<TaskStatus>> {
            Ok(None)
        }
    }

    async fn command_fixture(
        id: &str,
        labels: Vec<String>,
    ) -> (
        Arc<MockBackend>,
        Arc<dyn ExternalBackend>,
        Arc<TaskStore>,
        i64,
        Arc<TaskManager>,
        GhHttp,
        ExternalId,
    ) {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let store_id = store
            .create(&NewTask {
                external_id: Some(id.to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Task".to_string(),
                body: String::new(),
                source: "manual".to_string(),
                source_id: String::new(),
                author: "bot".to_string(),
                url: String::new(),
                labels: labels.clone(),
                parent_id: None,
            })
            .await
            .unwrap();
        let mock = Arc::new(MockBackend::with_labels(id, labels));
        let backend: Arc<dyn ExternalBackend> = Arc::<MockBackend>::clone(&mock);
        let task_manager = Arc::new(TaskManager::with_store(
            Arc::clone(&backend),
            Arc::clone(&store),
            "owner/repo".to_string(),
        ));
        (
            mock,
            backend,
            store,
            store_id,
            task_manager,
            GhHttp::new().unwrap(),
            ExternalId(id.to_string()),
        )
    }

    async fn run_command(
        backend: &Arc<dyn ExternalBackend>,
        gh: &GhHttp,
        store: &Arc<TaskStore>,
        task_manager: &Arc<TaskManager>,
        task_id: &ExternalId,
        command: OwnerCommand,
    ) -> anyhow::Result<String> {
        execute_command(
            backend,
            gh,
            "owner/repo",
            task_id,
            &command,
            &Some(Arc::clone(store) as Arc<dyn CommandStoreOps>),
            task_manager,
        )
        .await
    }

    #[tokio::test]
    async fn block_command_persists_reason_and_unblock_clears_it() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let store_id = store
            .create(&NewTask {
                external_id: Some("42".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Task".to_string(),
                body: String::new(),
                source: "manual".to_string(),
                source_id: String::new(),
                author: "bot".to_string(),
                url: String::new(),
                labels: vec![],
                parent_id: None,
            })
            .await
            .unwrap();
        let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new("42"));
        let backend_for_call = Arc::clone(&backend);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            Arc::clone(&store),
            "owner/repo".to_string(),
        ));
        let gh = GhHttp::new().unwrap();
        let task_id = ExternalId("42".to_string());

        execute_command(
            &backend_for_call,
            &gh,
            "owner/repo",
            &task_id,
            &OwnerCommand::Block(Some("waiting on upstream fix".to_string())),
            &Some(Arc::clone(&store) as Arc<dyn CommandStoreOps>),
            &task_manager,
        )
        .await
        .unwrap();

        let task = store.get(store_id).await.unwrap();
        assert_eq!(task.status, TaskStatus::Blocked);
        assert_eq!(
            task.block_reason.as_deref(),
            Some("waiting on upstream fix")
        );

        execute_command(
            &backend_for_call,
            &gh,
            "owner/repo",
            &task_id,
            &OwnerCommand::Unblock,
            &Some(Arc::clone(&store) as Arc<dyn CommandStoreOps>),
            &task_manager,
        )
        .await
        .unwrap();

        let task = store.get(store_id).await.unwrap();
        assert_eq!(task.status, TaskStatus::New);
        assert!(task.block_reason.is_none());
    }

    /// Done is terminal: `/retry`, `/reroute` and `/unblock` must refuse a done
    /// task instead of sending its stale body back to an agent (#3780).
    #[tokio::test]
    async fn done_task_refuses_retry_reroute_and_unblock() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let store_id = store
            .create(&NewTask {
                external_id: Some("58".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Task".to_string(),
                body: String::new(),
                source: "cron".to_string(),
                source_id: "code-development".to_string(),
                author: "bot".to_string(),
                url: String::new(),
                labels: vec![],
                parent_id: None,
            })
            .await
            .unwrap();
        store
            .update_status(store_id, TaskStatus::Done)
            .await
            .unwrap();

        let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new("58"));
        let task_manager = Arc::new(TaskManager::with_store(
            Arc::clone(&backend),
            Arc::clone(&store),
            "owner/repo".to_string(),
        ));
        let gh = GhHttp::new().unwrap();
        let task_id = ExternalId("58".to_string());

        for command in [
            OwnerCommand::Retry,
            OwnerCommand::Reroute(None),
            OwnerCommand::Unblock,
        ] {
            let err = execute_command(
                &backend,
                &gh,
                "owner/repo",
                &task_id,
                &command,
                &Some(Arc::clone(&store) as Arc<dyn CommandStoreOps>),
                &task_manager,
            )
            .await
            .expect_err("done task must not be revived");
            assert!(err.to_string().contains("done is terminal"), "got: {err}");
            assert_eq!(
                store.get(store_id).await.unwrap().status,
                TaskStatus::Done,
                "{command} must leave a done task done"
            );
        }
    }

    #[test]
    fn parse_retry() {
        assert_eq!(parse_command("/retry"), Some(OwnerCommand::Retry));
    }

    #[test]
    fn parse_retry_with_trailing_whitespace() {
        assert_eq!(parse_command("/retry   "), Some(OwnerCommand::Retry));
    }

    #[test]
    fn parse_close() {
        assert_eq!(parse_command("/close"), Some(OwnerCommand::Close));
    }

    #[test]
    fn parse_unblock() {
        assert_eq!(parse_command("/unblock"), Some(OwnerCommand::Unblock));
    }

    #[test]
    fn parse_review() {
        assert_eq!(parse_command("/review"), Some(OwnerCommand::Review));
    }

    #[test]
    fn parse_reroute_no_agent() {
        assert_eq!(parse_command("/reroute"), Some(OwnerCommand::Reroute(None)));
    }

    #[test]
    fn parse_reroute_with_agent() {
        assert_eq!(
            parse_command("/reroute codex"),
            Some(OwnerCommand::Reroute(Some("codex".into())))
        );
    }

    #[test]
    fn parse_block_no_reason() {
        assert_eq!(parse_command("/block"), Some(OwnerCommand::Block(None)));
    }

    #[test]
    fn parse_block_with_reason() {
        assert_eq!(
            parse_command("/block waiting on upstream fix"),
            Some(OwnerCommand::Block(Some("waiting on upstream fix".into())))
        );
    }

    #[test]
    fn parse_ignores_non_command_text() {
        assert_eq!(parse_command("This is a regular comment"), None);
    }

    #[test]
    fn parse_ignores_unknown_commands() {
        assert_eq!(parse_command("/unknown"), None);
    }

    #[test]
    fn parse_command_in_multiline_body() {
        let body = "Some context here\n\n/retry\n\nMore text";
        assert_eq!(parse_command(body), Some(OwnerCommand::Retry));
    }

    #[test]
    fn parse_first_valid_command_wins() {
        let body = "/retry\n/close";
        assert_eq!(parse_command(body), Some(OwnerCommand::Retry));
    }

    #[test]
    fn parse_skips_unknown_finds_valid() {
        let body = "/unknown\n/close";
        assert_eq!(parse_command(body), Some(OwnerCommand::Close));
    }

    #[test]
    fn parse_indented_command() {
        // Commands with leading whitespace should still be detected
        assert_eq!(parse_command("  /retry"), Some(OwnerCommand::Retry));
    }

    #[test]
    fn parse_mention_prefixed_command() {
        assert_eq!(parse_command("@orch /retry"), Some(OwnerCommand::Retry));
    }

    #[test]
    fn parse_legacy_mention_prefixed_command() {
        assert_eq!(
            parse_command("@orchestrator /close"),
            Some(OwnerCommand::Close)
        );
    }

    #[test]
    fn parse_ignores_non_command_after_mention() {
        assert_eq!(parse_command("@orch please retry this"), None);
    }

    #[test]
    fn parse_ignores_command_in_code_fence() {
        let body = "Some context\n```\n/retry\n```\nMore text";
        assert_eq!(parse_command(body), None);
    }

    #[test]
    fn parse_ignores_mention_prefixed_command_in_code_fence() {
        let body = "```\n@orch /retry\n```";
        assert_eq!(parse_command(body), None);
    }

    #[test]
    fn parse_ignores_command_in_tilde_fence() {
        let body = "Some context\n~~~\n/close\n~~~\nMore text";
        assert_eq!(parse_command(body), None);
    }

    #[test]
    fn parse_finds_command_after_code_fence() {
        let body = "```\n/retry\n```\n/close";
        assert_eq!(parse_command(body), Some(OwnerCommand::Close));
    }

    #[test]
    fn parse_ignores_command_in_code_fence_with_lang() {
        let body = "```markdown\n/retry\n```";
        assert_eq!(parse_command(body), None);
    }

    #[test]
    fn parse_mixed_fence_does_not_close_backtick_with_tilde() {
        // ~~~ should NOT close a ``` fence — /retry is still inside the fence
        let body = "```bash\n/retry\n~~~\n/retry\n```";
        assert_eq!(parse_command(body), None);
    }

    #[test]
    fn parse_mixed_fence_does_not_close_tilde_with_backtick() {
        // ``` should NOT close a ~~~ fence — /close is still inside the fence
        let body = "~~~\n/close\n```\n/close\n~~~";
        assert_eq!(parse_command(body), None);
    }

    #[test]
    fn parse_command_after_mixed_fence_properly_closed() {
        // Fence opened with ``` and closed with ``` (tilde in between is ignored)
        // Command after the real closing fence should be found
        let body = "```\n/retry\n~~~\n/retry\n```\n/close";
        assert_eq!(parse_command(body), Some(OwnerCommand::Close));
    }

    #[tokio::test]
    async fn retry_propagates_remove_label_error() {
        let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::failing_remove_label(
            "42",
            vec!["agent:claude".to_string()],
        ));
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let task_manager = Arc::new(TaskManager::with_store(
            Arc::clone(&backend),
            Arc::clone(&store),
            "owner/repo".to_string(),
        ));
        let gh = GhHttp::new().unwrap();
        let task_id = ExternalId("42".to_string());

        let result = execute_command(
            &backend,
            &gh,
            "owner/repo",
            &task_id,
            &OwnerCommand::Retry,
            &Some(store as Arc<dyn CommandStoreOps>),
            &task_manager,
        )
        .await;

        assert!(result.is_err(), "/retry must fail when remove_label fails");
        assert!(
            result.unwrap_err().to_string().contains("remove_label"),
            "error must mention remove_label"
        );
    }

    #[tokio::test]
    async fn reroute_propagates_remove_label_error() {
        let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::failing_remove_label(
            "42",
            vec!["agent:codex".to_string()],
        ));
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let task_manager = Arc::new(TaskManager::with_store(
            Arc::clone(&backend),
            Arc::clone(&store),
            "owner/repo".to_string(),
        ));
        let gh = GhHttp::new().unwrap();
        let task_id = ExternalId("42".to_string());

        let result = execute_command(
            &backend,
            &gh,
            "owner/repo",
            &task_id,
            &OwnerCommand::Reroute(None),
            &Some(store as Arc<dyn CommandStoreOps>),
            &task_manager,
        )
        .await;

        assert!(
            result.is_err(),
            "/reroute must fail when remove_label fails"
        );
        assert!(
            result.unwrap_err().to_string().contains("remove_label"),
            "error must mention remove_label"
        );
    }

    #[tokio::test]
    async fn block_propagates_resolve_task_id_error() {
        let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new("42"));
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let task_manager = Arc::new(TaskManager::with_store(
            Arc::clone(&backend),
            store,
            "owner/repo".to_string(),
        ));
        let gh = GhHttp::new().unwrap();
        let task_id = ExternalId("42".to_string());
        let failing_store: Arc<dyn CommandStoreOps> = Arc::new(FailingStore);

        let result = execute_command(
            &backend,
            &gh,
            "owner/repo",
            &task_id,
            &OwnerCommand::Block(Some("reason".to_string())),
            &Some(failing_store),
            &task_manager,
        )
        .await;

        assert!(
            result.is_err(),
            "/block must fail when resolve_task_id fails"
        );
        assert!(
            result.unwrap_err().to_string().contains("resolve_task_id"),
            "error must mention resolve_task_id"
        );
    }

    #[tokio::test]
    async fn unblock_propagates_resolve_task_id_error() {
        let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new("42"));
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let task_manager = Arc::new(TaskManager::with_store(
            Arc::clone(&backend),
            store,
            "owner/repo".to_string(),
        ));
        let gh = GhHttp::new().unwrap();
        let task_id = ExternalId("42".to_string());
        let failing_store: Arc<dyn CommandStoreOps> = Arc::new(FailingStore);

        let result = execute_command(
            &backend,
            &gh,
            "owner/repo",
            &task_id,
            &OwnerCommand::Unblock,
            &Some(failing_store),
            &task_manager,
        )
        .await;

        assert!(
            result.is_err(),
            "/unblock must fail when resolve_task_id fails"
        );
        assert!(
            result.unwrap_err().to_string().contains("resolve_task_id"),
            "error must mention resolve_task_id"
        );
    }

    #[test]
    fn parse_shorter_backtick_does_not_close_longer_backtick_fence() {
        // ```` opens a 4-backtick fence; ``` (3 backticks) must NOT close it
        let body = "````\n/retry\n```\n/retry\n````";
        assert_eq!(parse_command(body), None);
    }

    #[test]
    fn parse_longer_backtick_closes_shorter_backtick_fence() {
        // ``` opens a 3-backtick fence; ```` (4 backticks) IS allowed to close it
        let body = "```\n/retry\n````\n/close";
        assert_eq!(parse_command(body), Some(OwnerCommand::Close));
    }

    #[test]
    fn parse_shorter_tilde_does_not_close_longer_tilde_fence() {
        // ~~~~ opens a 4-tilde fence; ~~~ (3 tildes) must NOT close it
        let body = "~~~~\n/close\n~~~\n/close\n~~~~";
        assert_eq!(parse_command(body), None);
    }

    #[test]
    fn parse_longer_tilde_closes_shorter_tilde_fence() {
        // ~~~ opens a 3-tilde fence; ~~~~ (4 tildes) IS allowed to close it
        let body = "~~~\n/close\n~~~~\n/retry";
        assert_eq!(parse_command(body), Some(OwnerCommand::Retry));
    }

    #[test]
    fn parse_command_after_longer_backtick_fence_properly_closed() {
        // ```` fence closed with ```` — command after should be found
        let body = "````markdown\n/retry\n```\n/retry\n````\n/close";
        assert_eq!(parse_command(body), Some(OwnerCommand::Close));
    }

    #[test]
    fn display_commands() {
        assert_eq!(OwnerCommand::Retry.to_string(), "/retry");
        assert_eq!(
            OwnerCommand::Reroute(Some("codex".into())).to_string(),
            "/reroute codex"
        );
        assert_eq!(OwnerCommand::Reroute(None).to_string(), "/reroute");
        assert_eq!(OwnerCommand::Close.to_string(), "/close");
        assert_eq!(
            OwnerCommand::Block(Some("reason".into())).to_string(),
            "/block reason"
        );
        assert_eq!(OwnerCommand::Block(None).to_string(), "/block");
        assert_eq!(OwnerCommand::Unblock.to_string(), "/unblock");
        assert_eq!(OwnerCommand::Review.to_string(), "/review");
    }

    #[tokio::test]
    async fn retry_clears_agent_labels_resets_counters_and_returns_message() {
        let (mock, backend, store, store_id, task_manager, gh, task_id) =
            command_fixture("71", vec!["agent:claude".to_string(), "bug".to_string()]).await;
        store.increment(store_id, "attempts").await.unwrap();
        store.increment(store_id, "review_cycles").await.unwrap();

        let msg = run_command(
            &backend,
            &gh,
            &store,
            &task_manager,
            &task_id,
            OwnerCommand::Retry,
        )
        .await
        .unwrap();

        assert_eq!(
            msg,
            "`/retry` — reset attempts, cleared agent, reset to `status:new`"
        );
        let task = store.get(store_id).await.unwrap();
        assert_eq!(task.status, TaskStatus::New);
        assert_eq!(task.attempts, 0);
        assert_eq!(task.review_cycles, 0);
        assert_eq!(
            mock.removed_labels.lock().unwrap().as_slice(),
            ["agent:claude"]
        );
        assert!(mock.set_label_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn reroute_without_agent_clears_labels_and_returns_message() {
        let (mock, backend, store, store_id, task_manager, gh, task_id) =
            command_fixture("72", vec!["agent:codex".to_string()]).await;

        let msg = run_command(
            &backend,
            &gh,
            &store,
            &task_manager,
            &task_id,
            OwnerCommand::Reroute(None),
        )
        .await
        .unwrap();

        assert_eq!(
            msg,
            "`/reroute` — cleared agent, reset attempts, reset to `status:new`, will re-route"
        );
        assert_eq!(store.get(store_id).await.unwrap().status, TaskStatus::New);
        assert_eq!(
            mock.removed_labels.lock().unwrap().as_slice(),
            ["agent:codex"]
        );
        assert!(mock.set_label_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn reroute_with_agent_sets_forced_agent_label() {
        let (mock, backend, store, store_id, task_manager, gh, task_id) =
            command_fixture("73", vec!["agent:claude".to_string()]).await;

        let msg = run_command(
            &backend,
            &gh,
            &store,
            &task_manager,
            &task_id,
            OwnerCommand::Reroute(Some("codex".into())),
        )
        .await
        .unwrap();

        assert_eq!(
            msg,
            "`/reroute codex` — cleared agent, reset attempts, forced `agent:codex`, reset to `status:new`"
        );
        assert_eq!(store.get(store_id).await.unwrap().status, TaskStatus::New);
        assert_eq!(
            mock.removed_labels.lock().unwrap().as_slice(),
            ["agent:claude"]
        );
        assert_eq!(
            mock.set_label_calls.lock().unwrap().as_slice(),
            [vec!["agent:codex".to_string()]]
        );
    }

    #[tokio::test]
    async fn review_sets_needs_review_and_returns_message() {
        let (_mock, backend, store, store_id, task_manager, gh, task_id) =
            command_fixture("74", vec![]).await;

        let msg = run_command(
            &backend,
            &gh,
            &store,
            &task_manager,
            &task_id,
            OwnerCommand::Review,
        )
        .await
        .unwrap();

        assert_eq!(
            msg,
            "`/review` — set `status:needs_review`, review agent will pick up on next tick"
        );
        assert_eq!(
            store.get(store_id).await.unwrap().status,
            TaskStatus::NeedsReview
        );
    }

    #[tokio::test]
    async fn block_without_reason_returns_message() {
        let (_mock, backend, store, store_id, task_manager, gh, task_id) =
            command_fixture("75", vec![]).await;

        let msg = run_command(
            &backend,
            &gh,
            &store,
            &task_manager,
            &task_id,
            OwnerCommand::Block(None),
        )
        .await
        .unwrap();

        assert_eq!(msg, "`/block` — marked `status:blocked`");
        let task = store.get(store_id).await.unwrap();
        assert_eq!(task.status, TaskStatus::Blocked);
        assert!(task.block_reason.is_none());
    }
}
