//! GitHub Issues backend — uses `gh` CLI for all API calls.
//!
//! Auth is handled by `gh` (OAuth, tokens, SSO). No JWT, no token refresh,
//! no credential storage. Everyone who has `gh` installed can use orch.

use super::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::github::cli::GhCli;
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

    async fn update_status(&self, id: &ExternalId, status: Status) -> anyhow::Result<()> {
        // Atomic label replacement: GET current labels, swap status:* prefix,
        // PUT the full set in a single API call. No window where labels are missing.
        let task = self.get_task(id).await?;
        let mut labels: Vec<String> = task
            .labels
            .into_iter()
            .filter(|l| !l.starts_with("status:"))
            .collect();
        labels.push(status.as_label().to_string());
        self.gh.replace_labels(&self.repo, &id.0, &labels).await?;
        Ok(())
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

    async fn set_labels(&self, id: &ExternalId, labels: &[String]) -> anyhow::Result<()> {
        self.gh.add_labels(&self.repo, &id.0, labels).await
    }

    async fn remove_label(&self, id: &ExternalId, label: &str) -> anyhow::Result<()> {
        self.gh.remove_label(&self.repo, &id.0, label).await
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        self.gh.auth_status().await
    }
}
