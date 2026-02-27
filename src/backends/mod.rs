//! External backend abstraction — trait for issue trackers.
//!
//! GitHub Issues is the first implementation. The trait is designed so
//! Linear, Jira, GitLab, or any issue tracker can be swapped in later.

pub mod github;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Opaque identifier from the external system (issue number, Linear ID, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalId(pub String);

/// A mention/comment from the external system (backend-agnostic).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mention {
    /// Unique identifier for deduplication.
    pub id: String,
    /// The comment/mention body text.
    pub body: String,
    /// Author of the mention.
    pub author: String,
    /// When the mention was created (RFC 3339).
    pub created_at: String,
    /// API URL of the issue/PR this comment belongs to.
    /// Format: https://api.github.com/repos/owner/repo/issues/123
    pub issue_url: Option<String>,
}

/// A task as represented in the external system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalTask {
    pub id: ExternalId,
    pub title: String,
    pub body: String,
    pub state: String,
    pub labels: Vec<String>,
    pub author: String,
    pub created_at: String,
    pub updated_at: String,
    pub url: String,
}

/// Task status values understood by all backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    New,
    Routed,
    InProgress,
    Done,
    Blocked,
    InReview,
    NeedsReview,
}

impl Status {
    pub fn as_label(&self) -> &'static str {
        match self {
            Self::New => "status:new",
            Self::Routed => "status:routed",
            Self::InProgress => "status:in_progress",
            Self::Done => "status:done",
            Self::Blocked => "status:blocked",
            Self::InReview => "status:in_review",
            Self::NeedsReview => "status:needs_review",
        }
    }
}

/// The core trait for external task backends.
///
/// Each implementation talks to a different issue tracker.
/// The engine calls these methods without knowing which backend is active.
#[async_trait]
pub trait ExternalBackend: Send + Sync {
    /// Human-readable name (e.g. "github", "linear", "jira")
    fn name(&self) -> &str;

    /// Create a task in the external system.
    async fn create_task(
        &self,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> anyhow::Result<ExternalId>;

    /// Fetch a task by its external ID.
    async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask>;

    /// Update task status using idempotent remove + add operations.
    ///
    /// Default implementation retries up to 3 times with a fresh snapshot on
    /// each attempt to narrow the TOCTOU window. Backends that provide a native
    /// atomic status-update endpoint can override this.
    ///
    /// The retry loop:
    ///   1. Fetches a fresh label snapshot via `get_task`.
    ///   2. Removes all `status:*` labels except the target via `remove_label`.
    ///   3. Adds the target label via `set_labels`.
    ///
    /// Note: labels removed in a previous attempt are already gone — the retry
    /// re-evaluates the current state rather than undoing prior work.
    async fn update_status(&self, id: &ExternalId, status: Status) -> anyhow::Result<()> {
        let label = status.as_label();

        // Hook for backends that need to auto-create labels (e.g. GitHub).
        if let Err(e) = self.ensure_status_label(label).await {
            tracing::warn!(label, err = %e, "ensure_status_label failed, continuing");
        }

        const MAX_RETRIES: u32 = 3;
        let mut last_err =
            anyhow::anyhow!("update_status({label}) failed: all {MAX_RETRIES} attempts exhausted");

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(500 * u64::from(attempt)))
                    .await;
                tracing::debug!(attempt, label, "retrying update_status");
            }

            // Fresh snapshot on every attempt.
            let task = match self.get_task(id).await {
                Ok(t) => t,
                Err(e) => {
                    last_err = e;
                    continue;
                }
            };

            // Remove all existing status labels except the target.
            let mut remove_failed = false;
            for old in task
                .labels
                .iter()
                .filter(|l| l.starts_with("status:") && l.as_str() != label)
            {
                if let Err(e) = self.remove_label(id, old).await {
                    tracing::warn!(old_label = %old, err = %e, attempt, "remove_label failed");
                    last_err = e;
                    remove_failed = true;
                    break;
                }
            }
            if remove_failed {
                continue;
            }

            // Add the target status label.
            match self.set_labels(id, &[label.to_string()]).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!(label, err = %e, attempt, "set_labels failed");
                    last_err = e;
                }
            }
        }

        Err(last_err.context(format!(
            "update_status({label}) failed after {MAX_RETRIES} attempts"
        )))
    }

    /// List tasks by status.
    async fn list_by_status(&self, status: Status) -> anyhow::Result<Vec<ExternalTask>>;

    /// Post a comment / activity note.
    async fn post_comment(&self, id: &ExternalId, body: &str) -> anyhow::Result<()>;

    /// Add metadata labels / tags (additive — does not remove existing labels).
    /// The GitHub backend uses POST /repos/{repo}/issues/{number}/labels,
    /// which appends to the existing label set.
    #[allow(dead_code)]
    async fn set_labels(&self, id: &ExternalId, labels: &[String]) -> anyhow::Result<()>;

    /// Remove a label / tag.
    #[allow(dead_code)]
    async fn remove_label(&self, id: &ExternalId, label: &str) -> anyhow::Result<()>;

    /// Get sub-issues (children) of a task.
    ///
    /// Returns the list of external IDs for sub-issues.
    /// Empty list means no sub-issues or sub-issues not supported.
    async fn get_sub_issues(&self, id: &ExternalId) -> anyhow::Result<Vec<ExternalId>>;

    /// Create a child task and link it as a sub-issue of the parent.
    ///
    /// Creates the task via `create_task`, then attempts to establish
    /// a native sub-issue relationship. Falls back to `parent:{id}` label
    /// if the native link fails.
    async fn create_sub_task(
        &self,
        parent_id: &ExternalId,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> anyhow::Result<ExternalId> {
        // Default: create task with parent label, no native sub-issue link
        let mut all_labels = labels.to_vec();
        all_labels.push(format!("parent:{}", parent_id.0));
        self.create_task(title, body, &all_labels).await
    }

    /// Ensure a status label exists in the backend system.
    ///
    /// Called by the default `update_status` before applying labels.
    /// GitHub overrides this to auto-create labels on the repo.
    /// Default is a no-op.
    async fn ensure_status_label(&self, _label: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Check if an open issue with the given title already exists.
    ///
    /// Used for deduplication before creating issues. The `label` parameter
    /// narrows the search to issues with a specific label.
    /// Default returns `false` (no dedup for backends that don't support it).
    async fn has_open_issue_with_title(&self, _title: &str, _label: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    /// Check if connected and authenticated.
    async fn health_check(&self) -> anyhow::Result<()>;

    /// Check if a PR/merge request for the given branch has been merged.
    ///
    /// Returns false by default (backends that don't track PRs).
    async fn is_pr_merged(&self, _branch: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    /// Get the authenticated user's username.
    ///
    /// Returns None by default (backends that don't support user identity).
    async fn get_authenticated_user(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    /// Get recent mentions/comments since a given timestamp.
    ///
    /// Returns empty by default (backends that don't support mention scanning).
    async fn get_mentions(&self, _since: &str) -> anyhow::Result<Vec<Mention>> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    /// Mock backend that exercises the default trait `update_status` implementation.
    ///
    /// Does NOT override `update_status` — the trait default (retry loop with
    /// get_task → remove_label → set_labels) is what runs. This means tests
    /// verify the actual production orchestration logic, not a simplified mock.
    struct MockBackend {
        /// Current labels on the (single) tracked issue.
        labels: Arc<Mutex<Vec<String>>>,
        /// If set, `remove_label` returns this error for the matching label.
        remove_err: Option<String>,
        /// Counter for `set_labels` failures. Each call decrements; fails while > 0.
        set_labels_fail_count: Arc<Mutex<u32>>,
    }

    impl MockBackend {
        fn with_labels(labels: Vec<&str>) -> Self {
            Self {
                labels: Arc::new(Mutex::new(labels.into_iter().map(String::from).collect())),
                remove_err: None,
                set_labels_fail_count: Arc::new(Mutex::new(0)),
            }
        }

        fn current_labels(&self) -> Vec<String> {
            self.labels.lock().unwrap().clone()
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
            Ok(ExternalId("1".into()))
        }

        async fn get_task(&self, _id: &ExternalId) -> anyhow::Result<ExternalTask> {
            Ok(ExternalTask {
                id: ExternalId("1".into()),
                title: "mock".into(),
                body: "".into(),
                state: "open".into(),
                labels: self.current_labels(),
                author: "user".into(),
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
                url: "https://example.com".into(),
            })
        }

        // update_status is NOT overridden — uses the default trait implementation
        // which calls get_task, remove_label, set_labels with retry logic.

        async fn list_by_status(&self, _status: Status) -> anyhow::Result<Vec<ExternalTask>> {
            Ok(vec![])
        }

        async fn post_comment(&self, _id: &ExternalId, _body: &str) -> anyhow::Result<()> {
            Ok(())
        }

        async fn set_labels(&self, _id: &ExternalId, labels: &[String]) -> anyhow::Result<()> {
            let mut fail_count = self.set_labels_fail_count.lock().unwrap();
            if *fail_count > 0 {
                *fail_count -= 1;
                return Err(anyhow::anyhow!("transient network error"));
            }
            drop(fail_count);
            let mut current = self.labels.lock().unwrap();
            for l in labels {
                if !current.contains(l) {
                    current.push(l.clone());
                }
            }
            Ok(())
        }

        async fn remove_label(&self, _id: &ExternalId, label: &str) -> anyhow::Result<()> {
            if let Some(ref err_label) = self.remove_err {
                if label == err_label {
                    return Err(anyhow::anyhow!("rate limit (429)"));
                }
            }
            self.labels.lock().unwrap().retain(|l| l != label);
            Ok(())
        }

        async fn get_sub_issues(&self, _id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
            Ok(vec![])
        }

        async fn health_check(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    // --- Tests exercise the default trait update_status (retry + remove + add) ---

    #[tokio::test]
    async fn update_status_replaces_single_old_label() {
        let backend = MockBackend::with_labels(vec!["status:new", "priority:high"]);
        let id = ExternalId("1".into());
        backend
            .update_status(&id, Status::InProgress)
            .await
            .unwrap();
        let labels = backend.current_labels();
        assert!(
            labels.contains(&"status:in_progress".to_string()),
            "new label added: {labels:?}"
        );
        assert!(
            !labels.contains(&"status:new".to_string()),
            "old label removed: {labels:?}"
        );
        assert!(
            labels.contains(&"priority:high".to_string()),
            "non-status label preserved: {labels:?}"
        );
    }

    #[tokio::test]
    async fn update_status_removes_multiple_old_status_labels() {
        // Simulate state left by a previous partial failure: two status labels
        let backend = MockBackend::with_labels(vec!["status:in_progress", "status:blocked", "bug"]);
        let id = ExternalId("1".into());
        backend.update_status(&id, Status::Done).await.unwrap();
        let labels = backend.current_labels();
        let status_labels: Vec<_> = labels.iter().filter(|l| l.starts_with("status:")).collect();
        assert_eq!(
            status_labels,
            vec!["status:done"],
            "exactly one status label: {labels:?}"
        );
    }

    #[tokio::test]
    async fn update_status_is_idempotent() {
        let backend = MockBackend::with_labels(vec!["status:done"]);
        let id = ExternalId("1".into());
        backend.update_status(&id, Status::Done).await.unwrap();
        let labels = backend.current_labels();
        let status_count = labels.iter().filter(|l| l.starts_with("status:")).count();
        assert_eq!(
            status_count, 1,
            "idempotent: still exactly one status label: {labels:?}"
        );
    }

    #[tokio::test]
    async fn update_status_non_status_labels_are_preserved() {
        let backend =
            MockBackend::with_labels(vec!["status:routed", "agent:claude", "maintenance"]);
        let id = ExternalId("1".into());
        backend
            .update_status(&id, Status::InProgress)
            .await
            .unwrap();
        let labels = backend.current_labels();
        assert!(
            labels.contains(&"agent:claude".to_string()),
            "agent label preserved: {labels:?}"
        );
        assert!(
            labels.contains(&"maintenance".to_string()),
            "other label preserved: {labels:?}"
        );
    }

    #[tokio::test]
    async fn update_status_retries_on_transient_set_labels_failure() {
        let backend = MockBackend::with_labels(vec!["status:new"]);
        // set_labels fails once, then succeeds on retry
        *backend.set_labels_fail_count.lock().unwrap() = 1;
        let id = ExternalId("1".into());
        backend
            .update_status(&id, Status::InProgress)
            .await
            .unwrap();
        let labels = backend.current_labels();
        assert!(
            labels.contains(&"status:in_progress".to_string()),
            "label added after retry: {labels:?}"
        );
        assert!(
            !labels.contains(&"status:new".to_string()),
            "old label removed: {labels:?}"
        );
    }

    #[tokio::test]
    async fn update_status_fails_after_max_retries() {
        let backend = MockBackend::with_labels(vec!["status:new"]);
        // set_labels always fails (more failures than retries)
        *backend.set_labels_fail_count.lock().unwrap() = 10;
        let id = ExternalId("1".into());
        let result = backend.update_status(&id, Status::Done).await;
        assert!(result.is_err(), "should fail after max retries");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("failed after 3 attempts"),
            "error message mentions retry exhaustion"
        );
    }

    #[test]
    fn status_as_label() {
        assert_eq!(Status::New.as_label(), "status:new");
        assert_eq!(Status::Routed.as_label(), "status:routed");
        assert_eq!(Status::InProgress.as_label(), "status:in_progress");
        assert_eq!(Status::Done.as_label(), "status:done");
        assert_eq!(Status::Blocked.as_label(), "status:blocked");
        assert_eq!(Status::InReview.as_label(), "status:in_review");
        assert_eq!(Status::NeedsReview.as_label(), "status:needs_review");
    }

    #[test]
    fn status_serializes_snake_case() {
        let json = serde_json::to_string(&Status::InProgress).unwrap();
        assert_eq!(json, "\"in_progress\"");
    }

    #[test]
    fn status_deserializes_snake_case() {
        let status: Status = serde_json::from_str("\"needs_review\"").unwrap();
        assert_eq!(status, Status::NeedsReview);
    }

    #[test]
    fn external_id_clone() {
        let id = ExternalId("42".to_string());
        let cloned = id.clone();
        assert_eq!(id.0, cloned.0);
    }

    #[test]
    fn external_task_serializes() {
        let task = ExternalTask {
            id: ExternalId("1".to_string()),
            title: "Test".to_string(),
            body: "Body".to_string(),
            state: "open".to_string(),
            labels: vec!["status:new".to_string()],
            author: "user".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "https://github.com/test/test/issues/1".to_string(),
        };
        let json = serde_json::to_string(&task).unwrap();
        assert!(json.contains("\"title\":\"Test\""));
        assert!(json.contains("status:new"));
    }
}
