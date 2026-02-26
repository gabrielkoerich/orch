//! GitHub API response types — deserialized from `gh api` JSON output.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubIssue {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub labels: Vec<GitHubLabel>,
    pub user: GitHubUser,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: String,
    pub node_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubLabel {
    pub name: String,
    pub color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubUser {
    pub login: String,
}

#[allow(dead_code)] // used by list_comments and list_recent_comments
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubComment {
    pub id: u64,
    pub body: String,
    pub user: GitHubUser,
    pub created_at: String,
    pub updated_at: Option<String>,
    pub html_url: Option<String>,
    /// Present when fetched from the repo-level /issues/comments endpoint.
    /// Format: https://api.github.com/repos/owner/repo/issues/123
    pub issue_url: Option<String>,
}

/// PR review state - "APPROVED", "CHANGES_REQUESTED", "COMMENTED", "DISMISSED"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubReview {
    pub id: u64,
    pub user: GitHubUser,
    pub body: Option<String>,
    pub state: String,
    pub html_url: Option<String>,
    pub submitted_at: String,
    pub commit_id: Option<String>,
}

/// PR review comment on a specific line of code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubReviewComment {
    pub id: u64,
    pub user: GitHubUser,
    pub body: String,
    pub path: String,
    pub line: Option<u32>,
    pub original_line: Option<u32>,
    pub commit_id: String,
    pub original_commit_id: String,
    pub html_url: String,
    pub created_at: String,
    pub updated_at: String,
    pub in_reply_to_id: Option<u64>,
    pub diff_hunk: Option<String>,
}

/// A PR review with its associated comments.
#[derive(Debug, Clone)]
pub struct PullRequestReview {
    pub review: GitHubReview,
    pub comments: Vec<GitHubReviewComment>,
}

impl PullRequestReview {
    /// Check if this review requests changes.
    pub fn requests_changes(&self) -> bool {
        self.review.state == "CHANGES_REQUESTED"
    }

    /// Get actionable comments (non-empty, not replies).
    pub fn actionable_comments(&self) -> Vec<&GitHubReviewComment> {
        self.comments
            .iter()
            .filter(|c| !c.body.trim().is_empty() && c.in_reply_to_id.is_none())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_github_review_deserialization() {
        let json = r#"{
            "id": 789,
            "user": {"login": "reviewer"},
            "body": "Please fix this",
            "state": "CHANGES_REQUESTED",
            "html_url": "https://github.com/test/repo/pull/123#pullrequestreview-789",
            "submitted_at": "2024-01-01T12:00:00Z",
            "commit_id": "abc123"
        }"#;

        let review: GitHubReview = serde_json::from_str(json).unwrap();
        assert_eq!(review.id, 789);
        assert_eq!(review.user.login, "reviewer");
        assert_eq!(review.body, Some("Please fix this".to_string()));
        assert_eq!(review.state, "CHANGES_REQUESTED");
        assert_eq!(review.submitted_at, "2024-01-01T12:00:00Z");
        assert_eq!(review.commit_id, Some("abc123".to_string()));
    }

    #[test]
    fn test_github_review_comment_deserialization() {
        let json = r#"{
            "id": 101112,
            "user": {"login": "reviewer"},
            "body": "This line needs fixing",
            "path": "src/main.rs",
            "line": 42,
            "original_line": 42,
            "commit_id": "abc123",
            "original_commit_id": "abc123",
            "html_url": "https://github.com/test/repo/pull/123#discussion_r101112",
            "created_at": "2024-01-01T12:00:00Z",
            "updated_at": "2024-01-01T12:00:00Z",
            "in_reply_to_id": null,
            "diff_hunk": "@@ -40,5 +40,5 @@ fn main() {\n     let x = 1;\n-    let y = 2;\n+    let y = 3;"
        }"#;

        let comment: GitHubReviewComment = serde_json::from_str(json).unwrap();
        assert_eq!(comment.id, 101112);
        assert_eq!(comment.user.login, "reviewer");
        assert_eq!(comment.body, "This line needs fixing");
        assert_eq!(comment.path, "src/main.rs");
        assert_eq!(comment.line, Some(42));
        assert_eq!(comment.commit_id, "abc123");
        assert_eq!(comment.in_reply_to_id, None);
        assert!(comment.diff_hunk.is_some());
    }

    #[test]
    fn test_pull_request_review_requests_changes() {
        let review = PullRequestReview {
            review: GitHubReview {
                id: 1,
                user: GitHubUser {
                    login: "reviewer".to_string(),
                },
                body: Some("Please fix".to_string()),
                state: "CHANGES_REQUESTED".to_string(),
                html_url: None,
                submitted_at: "2024-01-01T00:00:00Z".to_string(),
                commit_id: None,
            },
            comments: vec![],
        };

        assert!(review.requests_changes());
    }

    #[test]
    fn test_pull_request_review_approved() {
        let review = PullRequestReview {
            review: GitHubReview {
                id: 1,
                user: GitHubUser {
                    login: "reviewer".to_string(),
                },
                body: Some("LGTM".to_string()),
                state: "APPROVED".to_string(),
                html_url: None,
                submitted_at: "2024-01-01T00:00:00Z".to_string(),
                commit_id: None,
            },
            comments: vec![],
        };

        assert!(!review.requests_changes());
    }

    #[test]
    fn test_pull_request_review_actionable_comments() {
        let review = PullRequestReview {
            review: GitHubReview {
                id: 1,
                user: GitHubUser {
                    login: "reviewer".to_string(),
                },
                body: None,
                state: "CHANGES_REQUESTED".to_string(),
                html_url: None,
                submitted_at: "2024-01-01T00:00:00Z".to_string(),
                commit_id: None,
            },
            comments: vec![
                GitHubReviewComment {
                    id: 1,
                    user: GitHubUser {
                        login: "reviewer".to_string(),
                    },
                    body: "Fix this".to_string(),
                    path: "src/main.rs".to_string(),
                    line: Some(10),
                    original_line: Some(10),
                    commit_id: "abc".to_string(),
                    original_commit_id: "abc".to_string(),
                    html_url: "url".to_string(),
                    created_at: "2024-01-01T00:00:00Z".to_string(),
                    updated_at: "2024-01-01T00:00:00Z".to_string(),
                    in_reply_to_id: None,
                    diff_hunk: None,
                },
                GitHubReviewComment {
                    id: 2,
                    user: GitHubUser {
                        login: "reviewer".to_string(),
                    },
                    body: "".to_string(), // Empty - not actionable
                    path: "src/lib.rs".to_string(),
                    line: Some(20),
                    original_line: Some(20),
                    commit_id: "abc".to_string(),
                    original_commit_id: "abc".to_string(),
                    html_url: "url".to_string(),
                    created_at: "2024-01-01T00:00:00Z".to_string(),
                    updated_at: "2024-01-01T00:00:00Z".to_string(),
                    in_reply_to_id: None,
                    diff_hunk: None,
                },
                GitHubReviewComment {
                    id: 3,
                    user: GitHubUser {
                        login: "reviewer".to_string(),
                    },
                    body: "Reply to this".to_string(),
                    path: "src/lib.rs".to_string(),
                    line: Some(30),
                    original_line: Some(30),
                    commit_id: "abc".to_string(),
                    original_commit_id: "abc".to_string(),
                    html_url: "url".to_string(),
                    created_at: "2024-01-01T00:00:00Z".to_string(),
                    updated_at: "2024-01-01T00:00:00Z".to_string(),
                    in_reply_to_id: Some(1), // Reply - not actionable as standalone
                    diff_hunk: None,
                },
            ],
        };

        let actionable = review.actionable_comments();
        assert_eq!(actionable.len(), 1);
        assert_eq!(actionable[0].id, 1);
        assert_eq!(actionable[0].body, "Fix this");
    }
}
