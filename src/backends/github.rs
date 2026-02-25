//! GitHub Issues backend — uses `gh` CLI for all API calls.
//!
//! Auth is handled by `gh` (OAuth, tokens, SSO). No JWT, no token refresh,
//! no credential storage. Everyone who has `gh` installed can use orch.

use super::{ExternalBackend, ExternalId, ExternalTask, Mention, Status};
use crate::github::cli::{status_label_color, GhCli};
use async_trait::async_trait;

pub struct GitHubBackend {
    repo: String,
    gh: GhCli,
}

impl GitHubBackend {
    pub fn new(repo: String) -> Self {
        Self {
            repo,
            gh: GhCli::new(),
        }
    }
}

#[async_trait]
impl ExternalBackend for GitHubBackend {
    fn name(&self) -> &str {
        "github"
    }

    async fn create_task(
        &self,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> anyhow::Result<ExternalId> {
        let issue = self
            .gh
            .create_issue(&self.repo, title, body, labels)
            .await?;
        Ok(ExternalId(issue.number.to_string()))
    }

    async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask> {
        let issue = self.gh.get_issue(&self.repo, &id.0).await?;
        Ok(ExternalTask {
            id: id.clone(),
            title: issue.title,
            body: issue.body.unwrap_or_default(),
            state: issue.state,
            labels: issue.labels.into_iter().map(|l| l.name).collect(),
            author: issue.user.login,
            created_at: issue.created_at,
            updated_at: issue.updated_at,
            url: issue.html_url,
        })
    }

    // update_status uses the default trait implementation (retry loop with
    // get_task → remove_label → set_labels). Only ensure_status_label is
    // overridden to auto-create labels on the GitHub repo.

    async fn ensure_status_label(&self, label: &str) -> anyhow::Result<()> {
        let status_name = &label["status:".len()..];
        self.gh
            .ensure_label(
                &self.repo,
                label,
                status_label_color(label),
                &format!("Task status: {status_name}"),
            )
            .await
    }

    async fn list_by_status(&self, status: Status) -> anyhow::Result<Vec<ExternalTask>> {
        let issues = self.gh.list_issues(&self.repo, status.as_label()).await?;
        Ok(issues
            .into_iter()
            .map(|issue| ExternalTask {
                id: ExternalId(issue.number.to_string()),
                title: issue.title,
                body: issue.body.unwrap_or_default(),
                state: issue.state,
                labels: issue.labels.into_iter().map(|l| l.name).collect(),
                author: issue.user.login,
                created_at: issue.created_at,
                updated_at: issue.updated_at,
                url: issue.html_url,
            })
            .collect())
    }

    async fn post_comment(&self, id: &ExternalId, body: &str) -> anyhow::Result<()> {
        self.gh.add_comment(&self.repo, &id.0, body).await
    }

    /// Additive: uses POST (gh.add_labels), so existing labels like bug, priority:high are preserved.
    async fn set_labels(&self, id: &ExternalId, labels: &[String]) -> anyhow::Result<()> {
        self.gh.add_labels(&self.repo, &id.0, labels).await
    }

    async fn remove_label(&self, id: &ExternalId, label: &str) -> anyhow::Result<()> {
        self.gh.remove_label(&self.repo, &id.0, label).await
    }

    async fn get_sub_issues(&self, id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
        let sub_issue_numbers = self.gh.get_sub_issues(&self.repo, &id.0).await?;
        Ok(sub_issue_numbers
            .into_iter()
            .map(|n| ExternalId(n.to_string()))
            .collect())
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        self.gh.auth_status().await
    }

    async fn is_pr_merged(&self, branch: &str) -> anyhow::Result<bool> {
        self.gh.is_pr_merged(&self.repo, branch).await
    }

    async fn get_authenticated_user(&self) -> anyhow::Result<Option<String>> {
        self.gh.get_whoami().await.map(Some)
    }

    async fn get_mentions(&self, since: &str) -> anyhow::Result<Vec<Mention>> {
        let comments = self.gh.get_mentions(&self.repo, since).await?;
        Ok(comments
            .into_iter()
            .map(|c| Mention {
                id: c.id.to_string(),
                body: c.body,
                author: c.user.login,
                created_at: c.created_at,
            })
            .collect())
    }
}
