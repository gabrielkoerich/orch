//! Owner slash commands — parse, validate, and execute /commands in issue comments.
//!
//! Provides command parsing (`parse_command`), validation + execution
//! (`validate_and_run_command`), and the individual command implementations.
//! The scanning loop lives in `sync.rs` (`scan_comments`).

use crate::backends::{ExternalBackend, ExternalId, Status};
use crate::github::http::GhHttp;
use crate::store;
use std::sync::Arc;

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

impl std::fmt::Display for OwnerCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
    // Track the opening fence character so mismatched closers are ignored.
    // Per CommonMark: a backtick fence closes only a backtick fence, and a
    // tilde fence closes only a tilde fence.
    let mut fence_char: Option<char> = None;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let ch = trimmed.chars().next().unwrap_or('\0');
            match fence_char {
                None => fence_char = Some(ch),
                Some(c) if c == ch => fence_char = None,
                Some(_) => {} // mismatched closer — ignore
            }
            continue;
        }
        if fence_char.is_some() {
            continue;
        }
        let command_text = trimmed
            .strip_prefix("@orchestrator")
            .or_else(|| trimmed.strip_prefix("@orch"))
            .map(str::trim_start)
            .unwrap_or(trimmed);

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
            _ => continue,
        }
    }
    None
}

/// Execute a single owner command against a task.
pub async fn execute_command(
    backend: &Arc<dyn ExternalBackend>,
    gh: &GhHttp,
    repo: &str,
    task_id: &ExternalId,
    command: &OwnerCommand,
    store: &Option<Arc<crate::store::TaskStore>>,
    task_manager: &Arc<crate::engine::tasks::TaskManager>,
) -> anyhow::Result<String> {
    match command {
        OwnerCommand::Retry => {
            // Remove agent labels, reset attempts and all failure counters, reset to new
            let task = backend.get_task(task_id).await?;
            for label in &task.labels {
                if label.starts_with("agent:") {
                    backend.remove_label(task_id, label).await.ok();
                }
            }
            // Reset store state (attempts + all failure counters) so the task starts fresh
            store::store_reset_counters(store, repo, &task_id.0).await;
            task_manager
                .update_task_status(task_id, Status::New)
                .await?;
            Ok("`/retry` — reset attempts, cleared agent, reset to `status:new`".to_string())
        }

        OwnerCommand::Reroute(agent) => {
            // Remove existing agent labels and reset attempts and all failure counters
            let task = backend.get_task(task_id).await?;
            for label in &task.labels {
                if label.starts_with("agent:") {
                    backend.remove_label(task_id, label).await.ok();
                }
            }
            // Reset store state (attempts + all failure counters) so the task starts fresh
            store::store_reset_counters(store, repo, &task_id.0).await;
            // Optionally set new agent
            if let Some(agent_name) = agent {
                let label = format!("agent:{agent_name}");
                backend.set_labels(task_id, &[label]).await?;
            }
            task_manager
                .update_task_status(task_id, Status::New)
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
            if let Some(ref store) = store {
                if let Ok(Some(store_id)) = store.resolve_task_id(repo, &task_id.0).await {
                    store.set_block_reason(store_id, reason.as_deref()).await?;
                }
            }
            task_manager
                .update_task_status(task_id, Status::Blocked)
                .await?;
            match reason {
                Some(r) => Ok(format!("`/block` — marked `status:blocked`: {r}")),
                None => Ok("`/block` — marked `status:blocked`".to_string()),
            }
        }

        OwnerCommand::Unblock => {
            if let Some(ref store) = store {
                if let Ok(Some(store_id)) = store.resolve_task_id(repo, &task_id.0).await {
                    store.set_block_reason(store_id, None).await?;
                }
            }
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
#[allow(clippy::too_many_arguments)]
pub async fn validate_and_run_command(
    backend: &Arc<dyn ExternalBackend>,
    gh: &GhHttp,
    repo: &str,
    issue_number: &str,
    command: &OwnerCommand,
    author: &str,
    store: &Option<Arc<crate::store::TaskStore>>,
    task_manager: &Arc<crate::engine::tasks::TaskManager>,
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
                crate::engine::orch_footer()
            );
            if let Err(e) = backend.post_comment(&task_id, &confirmation).await {
                tracing::warn!(issue = %issue_number, err = %e, "failed to post confirmation");
            }
        }
        Err(e) => {
            tracing::warn!(issue = %issue_number, command = %command, err = %e, "failed to execute command");
            let error_msg = format!(
                "[{now}] Failed to execute `{command}`: {e}{}",
                crate::engine::orch_footer()
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

    struct MockBackend {
        task: ExternalTask,
    }

    impl MockBackend {
        fn new(id: &str) -> Self {
            Self {
                task: ExternalTask {
                    id: ExternalId(id.to_string()),
                    title: "Task".to_string(),
                    body: "".to_string(),
                    state: "open".to_string(),
                    labels: vec![],
                    author: "bot".to_string(),
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                    updated_at: "2026-01-01T00:00:00Z".to_string(),
                    url: "".to_string(),
                },
            }
        }
    }

    #[async_trait]
    impl ExternalBackend for MockBackend {
        fn name(&self) -> &str {
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

        async fn set_labels(&self, _id: &ExternalId, _labels: &[String]) -> anyhow::Result<()> {
            Ok(())
        }

        async fn remove_label(&self, _id: &ExternalId, _label: &str) -> anyhow::Result<()> {
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

    #[tokio::test]
    async fn block_command_persists_reason_and_unblock_clears_it() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let store_id = store
            .create(&NewTask {
                external_id: Some("42".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Task".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "bot".to_string(),
                url: "".to_string(),
                labels: vec![],
                parent_id: None,
            })
            .await
            .unwrap();
        let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new("42"));
        let backend_for_call = backend.clone();
        let task_manager = Arc::new(crate::engine::tasks::TaskManager::with_store(
            backend,
            store.clone(),
            "owner/repo".to_string(),
        ));
        let gh = crate::github::http::GhHttp::new().unwrap();
        let task_id = ExternalId("42".to_string());

        execute_command(
            &backend_for_call,
            &gh,
            "owner/repo",
            &task_id,
            &OwnerCommand::Block(Some("waiting on upstream fix".to_string())),
            &Some(store.clone()),
            &task_manager,
        )
        .await
        .unwrap();

        let task = store.get(store_id).await.unwrap();
        assert_eq!(task.status, crate::store::TaskStatus::Blocked);
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
            &Some(store.clone()),
            &task_manager,
        )
        .await
        .unwrap();

        let task = store.get(store_id).await.unwrap();
        assert_eq!(task.status, crate::store::TaskStatus::New);
        assert!(task.block_reason.is_none());
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
}
