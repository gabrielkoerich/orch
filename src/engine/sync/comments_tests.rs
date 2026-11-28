use super::comments::{CommentAction, MentionCursor, classify_comment, handle_slash_command};
use super::*;
use crate::backends::test_helpers::NoopBackend;
use crate::backends::{ExternalId, ExternalTask, Mention, Status};
use crate::engine::tasks::commands::CommandGh;
use crate::engine::tasks::commands::OwnerCommand;
use crate::github::types::GitHubIssue;
use crate::github::types::GitHubUser;
use crate::store::TaskStore;
use async_trait::async_trait;
use serial_test::serial;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;

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
