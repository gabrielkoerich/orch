//! GitHub Issues backend — uses `gh` CLI for all API calls.
//!
//! Auth is handled by `gh` (OAuth, tokens, SSO). No JWT, no token refresh,
//! no credential storage. Everyone who has `gh` installed can use orch.

use super::{ExternalBackend, ExternalId, ExternalTask, Mention, Status};
use crate::github::cli::{status_label_color, GhCli};
use crate::github::projects::ProjectSync;
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

    /// Override default update_status to add project board sync after label update.
    async fn update_status(&self, id: &ExternalId, status: Status) -> anyhow::Result<()> {
        // Run the standard label-based status update (default trait impl logic)
        let label = status.as_label();

        if let Err(e) = self.ensure_status_label(label).await {
            tracing::warn!(label, err = %e, "ensure_status_label failed, continuing");
        }

        const MAX_RETRIES: u32 = 3;
        let mut last_err =
            anyhow::anyhow!("update_status({label}) failed: all {MAX_RETRIES} attempts exhausted");

        let mut issue_node_id: Option<String> = None;

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(500 * u64::from(attempt)))
                    .await;
                tracing::debug!(attempt, label, "retrying update_status");
            }

            let issue = match self.gh.get_issue(&self.repo, &id.0).await {
                Ok(i) => i,
                Err(e) => {
                    last_err = e;
                    continue;
                }
            };

            // Capture node_id for project sync
            if issue_node_id.is_none() {
                issue_node_id = issue.node_id.clone();
            }

            let labels: Vec<String> = issue.labels.iter().map(|l| l.name.clone()).collect();

            let mut remove_failed = false;
            for old in labels
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

            match self.set_labels(id, &[label.to_string()]).await {
                Ok(()) => {
                    // Label update succeeded — now sync project board (non-fatal)
                    if let Some(ref node_id) = issue_node_id {
                        if let Some(project) = ProjectSync::from_config() {
                            if let Err(e) = project.sync_item_status(node_id, &status).await {
                                tracing::warn!(
                                    task_id = id.0,
                                    err = %e,
                                    "project board sync failed (non-fatal)"
                                );
                            }
                        }
                    }
                    return Ok(());
                }
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

    async fn has_open_issue_with_title(&self, title: &str, label: &str) -> anyhow::Result<bool> {
        let issues = self.gh.list_issues(&self.repo, label).await?;
        Ok(issues.iter().any(|i| i.title == title))
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
