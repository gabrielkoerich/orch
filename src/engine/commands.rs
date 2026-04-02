//! Owner slash commands — detect and execute /commands in issue comments.
//!
//! Scans recent GitHub issue comments for slash commands (e.g. `/retry`,
//! `/close`, `/block reason`) posted by repo collaborators. Commands are
//! executed against the issue's task and a confirmation comment is posted.

use crate::backends::{ExternalBackend, ExternalId, Status};
use crate::github::http::GhHttp;
use crate::store;
use crate::store::TaskStore;
use std::sync::Arc;

/// Read a KV value from the store.
async fn kv_get(store: &Option<Arc<TaskStore>>, key: &str) -> Option<String> {
    if let Some(ref s) = store {
        if let Ok(v) = s.kv_get(key).await {
            return v;
        }
    }
    None
}

/// Write a KV value to the store.
async fn kv_set(store: &Option<Arc<TaskStore>>, key: &str, value: &str) {
    if let Some(ref s) = store {
        let _ = s.kv_set(key, value).await;
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
/// Scans each line for a `/command` at the start. Returns the first valid
/// command found. Unknown `/something` lines are skipped. Lines inside
/// markdown fenced code blocks (``` or ~~~) are ignored to prevent
/// accidental command execution from code examples. Per CommonMark, a
/// backtick fence closes only a backtick fence and a tilde fence closes
/// only a tilde fence — mismatched closers are ignored.
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
        if !trimmed.starts_with('/') {
            continue;
        }
        let parts: Vec<&str> = trimmed.splitn(2, char::is_whitespace).collect();
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

/// Extract issue number from a GitHub API issue URL.
///
/// Expected format: `https://api.github.com/repos/owner/repo/issues/123`
fn extract_issue_number(issue_url: &str) -> Option<String> {
    issue_url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        .map(String::from)
}

/// Scan recent comments for owner slash commands and execute them.
///
/// Uses a timestamp cursor (`owner_commands_last_checked`) for dedup.
/// Reuses the same comment endpoint as `scan_mentions`.
pub async fn scan_commands(
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    store: &Option<Arc<crate::store::TaskStore>>,
    task_manager: &Arc<crate::engine::tasks::TaskManager>,
) -> anyhow::Result<()> {
    let gh = GhHttp::new()?;

    // Use persisted cursor, fall back to 24h ago
    let fallback = chrono::Utc::now() - chrono::Duration::hours(24);
    let since_str = match kv_get(store, "owner_commands_last_checked").await {
        Some(ts) => ts,
        None => fallback.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    };

    // Fetch recent comments (same endpoint as mentions)
    let comments = match backend.get_mentions(&since_str).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(err = %e, "failed to fetch comments for command scanning");
            return Ok(());
        }
    };

    if comments.is_empty() {
        // Still advance cursor even if no comments
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        kv_set(store, "owner_commands_last_checked", &now).await;
        return Ok(());
    }

    // Build dedup set from already-processed command comment IDs.
    // We store processed IDs in KV to survive restarts within the cursor window.
    let processed_ids: std::collections::HashSet<String> =
        match kv_get(store, "owner_commands_processed_ids").await {
            Some(ids) if !ids.is_empty() => ids.split(',').map(String::from).collect(),
            _ => std::collections::HashSet::new(),
        };

    let mut new_processed = Vec::new();

    for mention in &comments {
        // Skip already processed
        if processed_ids.contains(&mention.id) {
            continue;
        }

        // Try to parse a command
        let command = match parse_command(&mention.body) {
            Some(cmd) => cmd,
            None => continue,
        };

        // Extract issue number from the comment's issue URL
        let issue_number = match mention.issue_url.as_deref().and_then(extract_issue_number) {
            Some(n) => n,
            None => {
                tracing::warn!(
                    comment_id = %mention.id,
                    "slash command without issue_url, skipping"
                );
                continue;
            }
        };

        // Only execute commands on open issues/PRs to prevent acting on
        // closed/merged issues (e.g. review comments with code examples).
        match gh.get_issue(repo, &issue_number).await {
            Ok(issue) if issue.state == "open" => {}
            Ok(issue) => {
                tracing::debug!(
                    issue = %issue_number,
                    state = %issue.state,
                    command = %command,
                    "ignoring slash command on non-open issue"
                );
                new_processed.push(mention.id.clone());
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    issue = %issue_number,
                    err = %e,
                    "failed to check issue state, skipping command"
                );
                continue;
            }
        }

        // Validate author is repo owner or collaborator
        match gh.is_collaborator(repo, &mention.author).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!(
                    author = %mention.author,
                    command = %command,
                    issue = %issue_number,
                    "ignoring slash command from non-collaborator"
                );
                new_processed.push(mention.id.clone());
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    author = %mention.author,
                    err = %e,
                    "failed to check collaborator status, skipping command"
                );
                continue;
            }
        }

        // Execute the command
        let task_id = ExternalId(issue_number.clone());
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

        let result =
            execute_command(backend, &gh, repo, &task_id, &command, store, task_manager).await;

        match result {
            Ok(msg) => {
                tracing::info!(
                    issue = %issue_number,
                    author = %mention.author,
                    command = %command,
                    "executed owner command"
                );
                let confirmation = format!(
                    "[{now}] {msg} — executed by @{}{}",
                    mention.author,
                    crate::engine::orch_footer()
                );
                if let Err(e) = backend.post_comment(&task_id, &confirmation).await {
                    tracing::warn!(issue = %issue_number, err = %e, "failed to post confirmation");
                }
            }
            Err(e) => {
                tracing::warn!(
                    issue = %issue_number,
                    command = %command,
                    err = %e,
                    "failed to execute owner command"
                );
                let error_msg = format!(
                    "[{now}] Failed to execute `{command}`: {e}{}",
                    crate::engine::orch_footer()
                );
                if let Err(e2) = backend.post_comment(&task_id, &error_msg).await {
                    tracing::warn!(issue = %issue_number, err = %e2, "failed to post error comment");
                }
            }
        }

        new_processed.push(mention.id.clone());
    }

    // Persist processed IDs (keep last 500 to avoid unbounded growth)
    if !new_processed.is_empty() {
        let mut all: Vec<String> = processed_ids.into_iter().collect();
        all.extend(new_processed);
        all.sort_by_key(|id| id.parse::<u64>().unwrap_or(0));
        if all.len() > 500 {
            all = all.split_off(all.len() - 500);
        }
        kv_set(store, "owner_commands_processed_ids", &all.join(",")).await;
    }

    // Advance cursor
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    kv_set(store, "owner_commands_last_checked", &now).await;

    Ok(())
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
    fn extract_issue_number_works() {
        assert_eq!(
            extract_issue_number("https://api.github.com/repos/owner/repo/issues/123"),
            Some("123".into())
        );
    }

    #[test]
    fn extract_issue_number_empty_url() {
        assert_eq!(extract_issue_number(""), None);
    }

    #[test]
    fn parse_ignores_command_in_code_fence() {
        let body = "Some context\n```\n/retry\n```\nMore text";
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

    // ── kv_get / kv_set store-first helpers ──────────────────────────

    #[tokio::test]
    async fn kv_get_reads_from_store() {
        let store = Arc::new(crate::store::TaskStore::open_memory().await.unwrap());

        store.kv_set("key1", "from_store").await.unwrap();

        let opt_store = Some(store);
        let val = kv_get(&opt_store, "key1").await;
        assert_eq!(val.as_deref(), Some("from_store"));
    }

    #[tokio::test]
    async fn kv_get_returns_none_without_store() {
        let opt_store: Option<Arc<crate::store::TaskStore>> = None;
        let val = kv_get(&opt_store, "key2").await;
        assert_eq!(val, None);
    }

    #[tokio::test]
    async fn kv_get_returns_none_for_missing_key() {
        let store = Arc::new(crate::store::TaskStore::open_memory().await.unwrap());

        let opt_store = Some(store);
        let val = kv_get(&opt_store, "nonexistent").await;
        assert_eq!(val, None);
    }

    #[tokio::test]
    async fn kv_set_writes_to_store_when_present() {
        let store = Arc::new(crate::store::TaskStore::open_memory().await.unwrap());

        let opt_store = Some(Arc::clone(&store));
        kv_set(&opt_store, "key3", "value3").await;

        assert_eq!(
            store.kv_get("key3").await.unwrap().as_deref(),
            Some("value3")
        );
    }

    #[tokio::test]
    async fn kv_set_noop_without_store() {
        let opt_store: Option<Arc<crate::store::TaskStore>> = None;
        // Should not panic
        kv_set(&opt_store, "key4", "value4").await;
    }
}
