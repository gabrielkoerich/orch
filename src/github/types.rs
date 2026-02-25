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

#[allow(dead_code)] // used by list_comments, not wired into engine yet
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubComment {
    pub id: u64,
    pub body: String,
    pub user: GitHubUser,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: Option<String>,
    pub issue_url: Option<String>,
}

/// GitHub Pull Request (simplified fields for orch use).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubPullRequest {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub user: GitHubUser,
    pub head: GitHubBranchRef,
    pub base: GitHubBranchRef,
    pub merged: Option<bool>,
    pub draft: Option<bool>,
    pub created_at: String,
    pub updated_at: String,
    pub html_url: String,
}

/// Branch reference in a PR.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubBranchRef {
    pub sha: String,
    pub ref_: String,
    #[serde(rename = "ref")]
    pub ref_field: String,
}
