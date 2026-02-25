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

    /// Update task status.
    async fn update_status(&self, id: &ExternalId, status: Status) -> anyhow::Result<()>;

    /// List tasks by status.
    async fn list_by_status(&self, status: Status) -> anyhow::Result<Vec<ExternalTask>>;

    /// Post a comment / activity note.
    async fn post_comment(&self, id: &ExternalId, body: &str) -> anyhow::Result<()>;

    /// Set metadata labels / tags.
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
