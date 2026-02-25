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
