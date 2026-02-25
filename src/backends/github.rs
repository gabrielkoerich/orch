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

    async fn update_status(&self, id: &ExternalId, status: Status) -> anyhow::Result<()> {
        // Ensure the target status label exists on the repo before assigning it.
        // Mirrors bash `_gh_ensure_label` — creates the label on first use so
        // callers never have to pre-create labels manually.  Failures are
        // tolerated: if creation fails we still attempt the assign below.
        let label = status.as_label();
        let status_name = &label["status:".len()..];
        if let Err(e) = self
            .gh
            .ensure_label(
                &self.repo,
                label,
                status_label_color(label),
                &format!("Task status: {status_name}"),
            )
            .await
        {
            tracing::warn!(label, err = %e, "ensure_label failed, continuing with label updates");
        }

        // Retry loop to narrow the TOCTOU window and handle transient failures.
        //
        // True atomicity is not achievable via the GitHub label API (no CAS
        // endpoint exists). Retrying with a fresh GET on each attempt reduces
        // the probability of leaving the task with duplicate or missing status
        // labels under concurrent modification.
        //
        // Error-handling contract:
        //   - `remove_label` already treats 404 as success (label already gone).
        //     We propagate all other errors rather than swallowing them, so
        //     auth failures, rate-limit responses, and network errors are surfaced.
        //   - If `add_labels` fails after successful removes, we retry the whole
        //     operation instead of leaving the task with no status label.
        const MAX_RETRIES: u32 = 3;
        let mut last_err = anyhow::anyhow!("update_status: no attempts made");

        for attempt in 0..MAX_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(500 * u64::from(attempt)))
                    .await;
                tracing::debug!(attempt, label, "retrying update_status");
            }

            // Fresh snapshot on every attempt to minimise the TOCTOU window.
            let task = match self.get_task(id).await {
                Ok(t) => t,
                Err(e) => {
                    last_err = e;
                    continue;
                }
            };

            // Remove all existing status labels except the target.
            // GhCli::remove_label treats 404 as success; all other errors propagate.
            let mut remove_failed = false;
            for old in task
                .labels
                .iter()
                .filter(|l| l.starts_with("status:") && l.as_str() != label)
            {
                if let Err(e) = self.gh.remove_label(&self.repo, &id.0, old).await {
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
            match self
                .gh
                .add_labels(&self.repo, &id.0, &[label.to_string()])
                .await
            {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!(label, err = %e, attempt, "add_labels failed");
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
