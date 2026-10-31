//! GitHub Issues backend — native HTTP via `reqwest` with connection pooling.
//!
//! Auth is resolved once at startup using the centralized [`TokenResolver`]
//! (`GH_TOKEN` → `GITHUB_TOKEN` environment variables by default).

use super::{ExternalBackend, ExternalId, ExternalTask, Mention, Status};
use crate::github::http::{GhHttp, status_label_color};
use crate::github::projects::ProjectSync;
use crate::github::projects::label_status::LabelSyncConfig;
use async_trait::async_trait;
use chrono::{Duration, Utc};

/// Author associations that are allowed to create tasks.
/// Only repository OWNER or COLLABORATOR authors are considered trusted for
/// ingestion. Other associations (members, collaborators, first-time
/// contributors, etc.) are ignored per the new sync policy.
/// Available values: COLLABORATOR, CONTRIBUTOR, FIRST_TIMER, FIRST_TIME_CONTRIBUTOR, MANNEQUIN, MEMBER, NONE, OWNER
// MEMBER is a member of the organization that owns the repo, as the operator is after a
// transfer to an org. Without it every new issue and comment there was dropped
const ALLOWED_ASSOCIATIONS: &[&str] = &["OWNER", "MEMBER", "COLLABORATOR"];

/// Check if an issue author is trusted
fn is_trusted_author(issue: &crate::github::types::GitHubIssue) -> bool {
    issue
        .author_association
        .as_deref()
        .is_some_and(|a| ALLOWED_ASSOCIATIONS.contains(&a)) // missing field = untrusted
}

fn is_trusted_comment_author(c: &crate::github::types::GitHubComment) -> bool {
    c.author_association
        .as_deref()
        .is_some_and(|a| ALLOWED_ASSOCIATIONS.contains(&a))
}

fn trusted_mentions(
    comments: impl IntoIterator<Item = crate::github::types::GitHubComment>,
) -> Vec<Mention> {
    comments
        .into_iter()
        .filter_map(|c| {
            if !is_trusted_comment_author(&c) {
                tracing::debug!(
                    comment_id = c.id,
                    author = %c.user.login,
                    "ignoring comment from untrusted author"
                );
                return None;
            }
            Some(Mention {
                id: c.id.to_string(),
                body: c.body,
                author: c.user.login,
                created_at: c.created_at,
                issue_url: c.issue_url,
            })
        })
        .collect()
}

fn active_status_from_label(label: &str) -> Option<Status> {
    match label {
        "status:routed" => Some(Status::Routed),
        "status:in_progress" => Some(Status::InProgress),
        "status:needs_review" => Some(Status::NeedsReview),
        "status:in_review" => Some(Status::InReview),
        "status:approved" => Some(Status::Approved),
        "status:blocked" => Some(Status::Blocked),
        "status:waiting" => Some(Status::Waiting),
        _ => None,
    }
}

fn classify_active_open_issue(
    issue: crate::github::types::GitHubIssue,
) -> Option<(ExternalTask, Option<Status>)> {
    // Guard: open issues tagged as done should never be re-ingested as new.
    if issue.labels.iter().any(|l| l.name == "status:done") {
        return None;
    }

    let status = issue
        .labels
        .iter()
        .find_map(|l| active_status_from_label(&l.name));
    let task = ExternalTask {
        id: ExternalId(issue.number.to_string()),
        title: issue.title,
        body: issue.body.unwrap_or_default(),
        state: issue.state,
        labels: issue.labels.into_iter().map(|l| l.name).collect(),
        author: issue.user.login,
        created_at: issue.created_at,
        updated_at: issue.updated_at,
        url: issue.html_url,
    };
    Some((task, status))
}

#[derive(Debug)]
pub struct GitHubBackend {
    repo: String,
    gh: GhHttp,
}

/// Returns `true` if `id` is a valid GitHub issue number (positive integer).
///
/// Internal tasks use IDs like `"internal:63857"` which are not valid GitHub
/// issue numbers. Backend methods that call the GitHub API must skip these IDs
/// to avoid spurious 404 errors.
fn is_github_issue_id(id: &ExternalId) -> bool {
    id.0.parse::<u64>().is_ok()
}

impl GitHubBackend {
    pub fn new(repo: String) -> anyhow::Result<Self> {
        Ok(Self {
            repo,
            gh: GhHttp::new()?,
        })
    }

    #[cfg(test)]
    fn for_test(repo: impl Into<String>, gh: GhHttp) -> Self {
        Self {
            repo: repo.into(),
            gh,
        }
    }

    async fn sync_project_status(
        &self,
        project: &ProjectSync,
        node_id: &str,
        status: &Status,
    ) -> anyhow::Result<()> {
        if LabelSyncConfig::from_config()?.manages(&self.repo, project.project_id()) {
            // Preserve existing ingestion membership behavior. The reconciler
            // owns the field value and reads the current labels on the next sync.
            project.add_item(node_id).await?;
            Ok(())
        } else {
            project.sync_item_status(node_id, status).await
        }
    }
}

#[async_trait]
impl ExternalBackend for GitHubBackend {
    fn name(&self) -> &'static str {
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

    /// Sync a task to the GitHub Project board (if configured).
    ///
    /// Called when a task is first ingested to ensure it appears on the
    /// project board immediately, not just when status changes later.
    /// Returns `Some(node_id)` if the item was added to the project board.
    async fn sync_to_project(
        &self,
        id: &ExternalId,
        status: Status,
    ) -> anyhow::Result<Option<String>> {
        // Only sync if project integration is configured
        let Some(project) = ProjectSync::from_config() else {
            return Ok(None);
        };

        // Fetch the issue to get its node_id
        let issue = self.gh.get_issue(&self.repo, &id.0).await?;

        // Get the node_id (required for project board operations)
        let node_id = issue
            .node_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("issue missing node_id"))?
            .to_string();

        // Add to project board and set initial status
        self.sync_project_status(&project, &node_id, &status)
            .await
            .map_err(|e| anyhow::anyhow!("project board sync failed: {e}"))?;

        Ok(Some(node_id))
    }

    async fn reconcile_project_statuses(&self) -> anyhow::Result<()> {
        // Projects V2 on a personal account is out of reach for an installation token
        LabelSyncConfig::from_config()?
            .reconcile(&GhHttp::owner()?, &self.repo)
            .await
            .map(|_| ())
    }

    async fn sync_estimate_to_project(&self, id: &ExternalId, estimate: u8) -> anyhow::Result<()> {
        let Some(project) = ProjectSync::from_config() else {
            return Ok(());
        };

        // If the estimate field ID is missing from config, try to discover it
        // automatically and persist it for future calls. This handles the case
        // where the service was set up before the estimate sync feature existed.
        let project = if project.estimate_field_id().is_none() {
            match ProjectSync::discover_fields(project.project_id()).await {
                Ok(discovered) => {
                    if discovered.estimate_field_id().is_some() {
                        match crate::github::projects::write_project_config(&discovered).await {
                            Err(e) => {
                                tracing::warn!(
                                    err = %e,
                                    "failed to persist discovered estimate field ID to config"
                                );
                            }
                            _ => {
                                tracing::info!(
                                    "auto-discovered and persisted project_estimate_field_id to config"
                                );
                            }
                        }
                        discovered
                    } else {
                        tracing::debug!(
                            "project has no 'Estimate' number field — skipping estimate sync"
                        );
                        return Ok(());
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        err = %e,
                        "estimate field auto-discovery failed — skipping estimate sync"
                    );
                    return Ok(());
                }
            }
        } else {
            project
        };

        // Fetch the issue to get its node_id.
        let issue = self.gh.get_issue(&self.repo, &id.0).await?;
        let node_id = issue
            .node_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("issue missing node_id"))?;

        project
            .sync_item_estimate(node_id, estimate)
            .await
            .map_err(|e| anyhow::anyhow!("project estimate sync failed: {e}"))?;

        Ok(())
    }

    async fn get_project_item_estimates(
        &self,
        issue_node_ids: &[String],
    ) -> anyhow::Result<std::collections::HashMap<String, u8>> {
        let Some(project) = ProjectSync::from_config() else {
            return Ok(std::collections::HashMap::new());
        };

        if issue_node_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        project
            .get_estimates_for_issues(issue_node_ids)
            .await
            .map_err(|e| anyhow::anyhow!("project estimate fetch failed: {e}"))
    }

    /// Override default update_status to add project board sync after label update.
    async fn update_status(&self, id: &ExternalId, status: Status) -> anyhow::Result<()> {
        const MAX_RETRIES: u32 = 3;
        // Internal tasks (e.g. "internal:63857") are not GitHub issues — skip silently.
        if !is_github_issue_id(id) {
            return Ok(());
        }

        // Run the standard label-based status update (default trait impl logic)
        let label = status.as_label();

        if let Err(e) = self.ensure_status_label(label).await {
            tracing::warn!(label, err = %e, "ensure_status_label failed, continuing");
        }

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
                    if let Some(ref node_id) = issue_node_id
                        && let Some(project) = ProjectSync::from_config()
                        && let Err(e) = self.sync_project_status(&project, node_id, &status).await
                    {
                        tracing::warn!(
                            task_id = id.0,
                            err = %e,
                            "project board sync failed (non-fatal)"
                        );
                    }

                    // Auto-close issue when status → Done (if auto_close enabled)
                    if status == Status::Done {
                        let auto_close =
                            crate::config::get("workflow.auto_close").map_or(true, |v| v == "true");
                        if auto_close && let Err(e) = self.gh.close_issue(&self.repo, &id.0).await {
                            tracing::warn!(
                                task_id = id.0,
                                err = %e,
                                "auto-close issue failed (non-fatal)"
                            );
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
        // Done issues are auto-closed by the engine, so we must query state=all
        // to find them. All other statuses only live on open issues.
        let state = if status == Status::Done {
            "all"
        } else {
            "open"
        };
        let issues = self
            .gh
            .list_issues_with_state(&self.repo, status.as_label(), state)
            .await?;
        Ok(issues
            .into_iter()
            .filter(|issue| issue.pull_request.is_none()) // Exclude PRs
            .filter(is_trusted_author) // Only trusted authors
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

    /// List all tasks (open and closed) in a single API call.
    ///
    /// This fetches all issues with state=all and filters by status labels locally.
    /// More efficient than making 7 separate calls to `list_by_status`.
    async fn list_all_tasks(&self) -> anyhow::Result<Vec<ExternalTask>> {
        let issues = self.gh.list_all_issues(&self.repo).await?;
        Ok(issues
            .into_iter()
            .filter(|issue| issue.pull_request.is_none()) // Exclude PRs
            .filter(is_trusted_author) // Only trusted authors
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

    async fn list_reconciliation_candidates(&self) -> anyhow::Result<Vec<ExternalTask>> {
        let since = (Utc::now() - Duration::days(30)).to_rfc3339();
        let closed = self.gh.list_closed_issues_since(&self.repo, &since).await?;

        Ok(closed
            .into_iter()
            .filter(|issue| issue.pull_request.is_none()) // Exclude PRs
            .filter(is_trusted_author) // Only trusted authors
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

    /// List open issues that have no `status:*` label (unprocessed) or have `status:new`.
    async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
        let issues = self.gh.list_all_open_issues(&self.repo).await?;
        Ok(issues
            .into_iter()
            .filter(|issue| issue.pull_request.is_none()) // Exclude PRs
            .filter(is_trusted_author) // Only trusted authors
            .filter(|issue| {
                let labels: Vec<&str> = issue.labels.iter().map(|l| l.name.as_str()).collect();
                // Include if: has status:new, OR has no status:* label at all
                let has_status = labels.iter().any(|l| l.starts_with("status:"));
                !has_status || labels.contains(&"status:new")
            })
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

    /// Single-call override: fetches all open issues once and partitions by status label locally,
    /// replacing the 1 (list_routable) + 5 (list_by_status per active status) fan-out with a
    /// single `list_all_open_issues` request.
    async fn list_active_open_issues(
        &self,
    ) -> anyhow::Result<Vec<(ExternalTask, Option<crate::backends::Status>)>> {
        let issues = self.gh.list_all_open_issues(&self.repo).await?;

        Ok(issues
            .into_iter()
            .filter(|issue| issue.pull_request.is_none()) // Exclude PRs
            .filter(is_trusted_author) // Only trusted authors
            .filter_map(classify_active_open_issue)
            .collect())
    }

    async fn post_comment(&self, id: &ExternalId, body: &str) -> anyhow::Result<()> {
        // Internal tasks (e.g. "internal:63857") are not GitHub issues — skip silently.
        if !is_github_issue_id(id) {
            return Ok(());
        }
        self.gh.add_comment(&self.repo, &id.0, body).await
    }

    /// Additive: uses POST (gh.add_labels), so existing labels like bug, priority:high are preserved.
    async fn set_labels(&self, id: &ExternalId, labels: &[String]) -> anyhow::Result<()> {
        // Internal tasks (e.g. "internal:63857") are not GitHub issues — skip silently.
        if !is_github_issue_id(id) {
            return Ok(());
        }
        self.gh.add_labels(&self.repo, &id.0, labels).await
    }

    async fn remove_label(&self, id: &ExternalId, label: &str) -> anyhow::Result<()> {
        // Internal tasks (e.g. "internal:63857") are not GitHub issues — skip silently.
        if !is_github_issue_id(id) {
            return Ok(());
        }
        self.gh.remove_label(&self.repo, &id.0, label).await
    }

    async fn get_sub_issues(&self, id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
        let sub_issue_numbers = self.gh.get_sub_issues(&self.repo, &id.0).await?;
        Ok(sub_issue_numbers
            .into_iter()
            .map(|n| ExternalId(n.to_string()))
            .collect())
    }

    async fn create_sub_task(
        &self,
        parent_id: &ExternalId,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> anyhow::Result<ExternalId> {
        // Add parent label for metadata
        let mut all_labels = labels.to_vec();
        all_labels.push(format!("parent:{}", parent_id.0));

        // Create the child issue
        let child_issue = self
            .gh
            .create_issue(&self.repo, title, body, &all_labels)
            .await?;

        let child_id = ExternalId(child_issue.number.to_string());

        // Try to establish native sub-issue relationship via GraphQL
        if let (Some(child_node_id), Ok(parent_issue)) = (
            child_issue.node_id,
            self.gh.get_issue(&self.repo, &parent_id.0).await,
        ) && let Some(parent_node_id) = parent_issue.node_id
            && let Err(e) = self.gh.add_sub_issue(&parent_node_id, &child_node_id).await
        {
            tracing::warn!(
                parent = parent_id.0,
                child = child_id.0,
                err = %e,
                "failed to add native sub-issue link (parent label still set)"
            );
        }

        Ok(child_id)
    }

    async fn has_open_issue_with_title(&self, title: &str, label: &str) -> anyhow::Result<bool> {
        // Check open issues first
        let open_issues = if label.is_empty() {
            self.gh.list_all_open_issues(&self.repo).await?
        } else {
            self.gh.list_issues(&self.repo, label).await?
        };
        if open_issues.iter().any(|i| i.title == title) {
            return Ok(true);
        }
        // Also check issues closed within the last 24h — prevents re-creating a bug
        // that was just fixed and merged (the PR close event removes the open issue).
        let since = (chrono::Utc::now() - chrono::Duration::hours(24))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let closed = if label.is_empty() {
            self.gh.list_closed_issues_since(&self.repo, &since).await?
        } else {
            self.gh
                .list_issues_closed_since(&self.repo, label, &since)
                .await?
        };
        Ok(closed.iter().any(|i| i.title == title))
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        self.gh.auth_status().await
    }

    async fn is_pr_merged(&self, branch: &str) -> anyhow::Result<bool> {
        self.gh.is_pr_merged(&self.repo, branch).await
    }

    async fn batch_is_pr_merged(
        &self,
        branches: &[String],
    ) -> anyhow::Result<std::collections::HashMap<String, bool>> {
        self.gh
            .batch_is_pr_merged_by_branch(&self.repo, branches)
            .await
    }

    async fn get_authenticated_user(&self) -> anyhow::Result<Option<String>> {
        self.gh.get_whoami().await.map(Some)
    }

    async fn get_mentions(&self, since: &str) -> anyhow::Result<Vec<Mention>> {
        let comments = self.gh.get_mentions(&self.repo, since).await?;
        Ok(trusted_mentions(comments))
    }

    async fn list_issue_comments(&self, id: &ExternalId) -> anyhow::Result<Vec<Mention>> {
        let issue_number: u64 =
            id.0.parse()
                .map_err(|_| anyhow::anyhow!("invalid issue number: {}", id.0))?;
        let comments = self.gh.get_issue_comments(&self.repo, issue_number).await?;
        Ok(trusted_mentions(comments))
    }

    async fn acknowledge_issue(&self, id: &ExternalId) -> anyhow::Result<()> {
        self.gh.add_reaction(&self.repo, &id.0, "eyes").await
    }

    async fn acknowledge_mention(&self, comment_id: &str) -> anyhow::Result<()> {
        self.gh
            .add_comment_reaction(&self.repo, comment_id, "eyes")
            .await
    }

    async fn blocked_by(
        &self,
        repo: &str,
        issue_number: i64,
    ) -> anyhow::Result<crate::backends::BlockedByFetch> {
        if issue_number <= 0 {
            return Ok(crate::backends::BlockedByFetch::Unavailable);
        }
        self.gh
            .list_blocked_by(repo, issue_number.cast_unsigned())
            .await
    }

    async fn issue_close_state(
        &self,
        repo: &str,
        issue_number: i64,
    ) -> anyhow::Result<Option<(String, Option<String>)>> {
        if issue_number <= 0 {
            return Ok(None);
        }
        match self.gh.get_issue(repo, &issue_number.to_string()).await {
            Ok(issue) => Ok(Some((issue.state, issue.state_reason))),
            Err(err) if is_missing_issue(&err) => Ok(None),
            Err(err) => Err(err),
        }
    }

    async fn post_comment_in_repo(
        &self,
        repo: &str,
        id: &ExternalId,
        body: &str,
    ) -> anyhow::Result<()> {
        if !is_github_issue_id(id) {
            return Ok(());
        }
        self.gh.add_comment(repo, &id.0, body).await
    }

    async fn pull_request_completion(
        &self,
        repo: &str,
        pr_number: i64,
    ) -> anyhow::Result<Option<crate::backends::PullRequestCompletion>> {
        if pr_number <= 0 {
            return Ok(None);
        }
        let pr = match self.gh.get_pr(repo, pr_number.cast_unsigned()).await {
            Ok(pr) => pr,
            Err(err) if is_missing_issue(&err) => return Ok(None),
            Err(err) => return Err(err),
        };
        let default_branch = self.gh.get_default_branch(repo).await.unwrap_or_default();
        let app_login = crate::github::app::bot_login().await;
        let reviews = match self
            .gh
            .get_pr_reviews(repo, pr_number.cast_unsigned())
            .await
        {
            Ok(reviews) => {
                crate::github::app::reviews_for_gate(&reviews, &pr.head.sha, app_login.as_deref())
            }
            Err(err) => {
                tracing::warn!(repo, pr_number, err = %err, "pull request reviews unreadable");
                Vec::new()
            }
        };
        let comments = match self.gh.list_comments(repo, &pr_number.to_string()).await {
            Ok(comments) => comments,
            Err(err) => {
                tracing::warn!(repo, pr_number, err = %err, "pull request comments unreadable");
                Vec::new()
            }
        };
        let reviewer = match self.gh.get_whoami().await {
            Ok(login) => Some(login),
            Err(err) => {
                tracing::warn!(repo, pr_number, err = %err, "reviewer login unreadable");
                None
            }
        };
        let comment_refs: Vec<AutomatedReviewComment<'_>> = comments
            .iter()
            .map(|comment| AutomatedReviewComment {
                login: &comment.user.login,
                body: &comment.body,
                created_at: &comment.created_at,
            })
            .collect();
        let comment_approved = reviewer
            .iter()
            .chain(app_login.iter())
            .any(|login| latest_automated_review_approves(&comment_refs, login));
        // An App review on the head replaces the comment, as in review_poll, so a stale
        // approving comment cannot outvote an App change request
        let (comment_approved, _) = crate::github::app::effective_comment_flags(
            &reviews,
            &pr.head.sha,
            app_login.as_deref(),
            comment_approved,
            false,
        );
        let merged = pr.merged.unwrap_or(false);
        Ok(Some(crate::backends::PullRequestCompletion {
            merged,
            base_ref: pr.base.ref_,
            default_branch,
            approved: latest_review_approves(&reviews) || comment_approved,
            closed: pr.state.eq_ignore_ascii_case("closed"),
        }))
    }

    async fn add_dependency(
        &self,
        issue_number: i64,
        blocking_repo: &str,
        blocking_number: i64,
    ) -> anyhow::Result<()> {
        let blocker = self
            .gh
            .get_issue(blocking_repo, &blocking_number.to_string())
            .await?;
        if blocker.id == 0 {
            anyhow::bail!("blocking issue {blocking_repo}#{blocking_number} has no database id");
        }
        self.gh
            .add_blocked_by(
                &self.repo,
                issue_number.cast_unsigned(),
                blocker.id.cast_signed(),
            )
            .await
    }
}

fn is_missing_issue(err: &anyhow::Error) -> bool {
    let text = err.to_string().to_ascii_lowercase();
    text.contains("404") || text.contains("not found")
}

/// Latest substantive review wins. Comments and dismissed reviews are ignored.
///
/// `CHANGES_REQUESTED` after `APPROVED` means the pull request is not approved
pub(crate) fn latest_review_approves(reviews: &[crate::github::types::GitHubReview]) -> bool {
    let mut latest: Option<(&str, &str)> = None;
    for review in reviews {
        let state = review.state.as_str();
        let substantive = state.eq_ignore_ascii_case("APPROVED")
            || state.eq_ignore_ascii_case("CHANGES_REQUESTED");
        if !substantive {
            continue;
        }
        match latest {
            Some((at, _)) if review.submitted_at.as_str() < at => {}
            _ => latest = Some((review.submitted_at.as_str(), state)),
        }
    }
    latest.is_some_and(|(_, state)| state.eq_ignore_ascii_case("APPROVED"))
}

/// One issue comment considered for automated review approval.
pub(crate) struct AutomatedReviewComment<'a> {
    pub login: &'a str,
    pub body: &'a str,
    pub created_at: &'a str,
}

/// First-line verdict of a `## Automated Review` comment.
///
/// Orch writes an em dash. The review gate also accepts a hyphen
pub(crate) fn automated_review_verdict(body: &str) -> Option<&'static str> {
    let first = body.lines().next().unwrap_or("").trim().replace('\r', "");
    if !first.starts_with("## Automated Review") {
        return None;
    }
    let normalized = first.replace(['\u{2014}', '\u{2013}'], "-");
    if normalized.contains("Automated Review - Approve") {
        Some("approve")
    } else if normalized.contains("Automated Review - Changes Requested") {
        Some("changes_requested")
    } else {
        None
    }
}

/// Latest `## Automated Review` comment from `reviewer` wins.
///
/// An empty reviewer matches nothing. A later changes-requested comment
/// clears an earlier approve
pub(crate) fn latest_automated_review_approves(
    comments: &[AutomatedReviewComment<'_>],
    reviewer: &str,
) -> bool {
    if reviewer.is_empty() {
        return false;
    }
    let mut newest: Option<&AutomatedReviewComment<'_>> = None;
    for comment in comments {
        if automated_review_verdict(comment.body).is_none() {
            continue;
        }
        if !comment.login.eq_ignore_ascii_case(reviewer) {
            continue;
        }
        match newest {
            Some(prev) if comment.created_at <= prev.created_at => {}
            _ => newest = Some(comment),
        }
    }
    newest.is_some_and(|comment| automated_review_verdict(comment.body) == Some("approve"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::types::{GitHubIssue, GitHubReview, GitHubUser};

    fn review(state: &str, at: &str) -> GitHubReview {
        GitHubReview {
            id: 1,
            user: GitHubUser {
                login: "owner".into(),
            },
            body: None,
            state: state.into(),
            html_url: None,
            submitted_at: at.into(),
            commit_id: None,
        }
    }

    #[test]
    fn later_changes_requested_clears_an_earlier_approval() {
        let reviews = vec![
            review("APPROVED", "2026-01-01T00:00:00Z"),
            review("COMMENTED", "2026-01-02T00:00:00Z"),
            review("CHANGES_REQUESTED", "2026-01-03T00:00:00Z"),
        ];
        assert!(!latest_review_approves(&reviews));
        let approved = vec![
            review("CHANGES_REQUESTED", "2026-01-01T00:00:00Z"),
            review("APPROVED", "2026-01-02T00:00:00Z"),
        ];
        assert!(latest_review_approves(&approved));
    }

    fn review_comment<'a>(
        login: &'a str,
        body: &'a str,
        at: &'a str,
    ) -> AutomatedReviewComment<'a> {
        AutomatedReviewComment {
            login,
            body,
            created_at: at,
        }
    }

    #[test]
    fn automated_review_comment_counts_as_approval_without_formal_reviews() {
        assert!(!latest_review_approves(&[]));
        let comments = [review_comment(
            "orch-bot",
            "## Automated Review \u{2014} Approve\n\nLooks good.",
            "2026-01-02T00:00:00Z",
        )];
        assert!(latest_automated_review_approves(&comments, "orch-bot"));
        assert!(latest_automated_review_approves(&comments, "Orch-Bot"));
        assert!(!latest_automated_review_approves(&comments, "mallory"));
        assert!(!latest_automated_review_approves(&comments, ""));
        let hyphen = [review_comment(
            "orch-bot",
            "## Automated Review - Approve\n\nok",
            "2026-01-02T00:00:00Z",
        )];
        assert!(latest_automated_review_approves(&hyphen, "orch-bot"));
    }

    #[test]
    fn later_automated_changes_requested_clears_comment_approval() {
        let comments = [
            review_comment(
                "orch-bot",
                "## Automated Review \u{2014} Approve\n\nok",
                "2026-01-01T00:00:00Z",
            ),
            review_comment(
                "mallory",
                "## Automated Review \u{2014} Changes Requested\n\nnope",
                "2026-01-03T00:00:00Z",
            ),
            review_comment(
                "orch-bot",
                "## Automated Review \u{2014} Changes Requested\n\nfix",
                "2026-01-02T00:00:00Z",
            ),
        ];
        assert!(!latest_automated_review_approves(&comments, "orch-bot"));
    }

    // --- is_github_issue_id guard ---

    #[test]
    fn is_github_issue_id_numeric() {
        assert!(is_github_issue_id(&ExternalId("42".to_string())));
        assert!(is_github_issue_id(&ExternalId("1".to_string())));
        assert!(is_github_issue_id(&ExternalId("99999".to_string())));
    }

    #[test]
    fn is_github_issue_id_rejects_internal_prefix() {
        assert!(!is_github_issue_id(&ExternalId(
            "internal:63857".to_string()
        )));
        assert!(!is_github_issue_id(&ExternalId("internal:1".to_string())));
    }

    #[test]
    fn is_github_issue_id_rejects_empty_and_non_numeric() {
        assert!(!is_github_issue_id(&ExternalId(String::new())));
        assert!(!is_github_issue_id(&ExternalId("abc".to_string())));
        assert!(!is_github_issue_id(&ExternalId("1.0".to_string())));
    }

    // ---

    #[test]
    fn allowed_associations_are_owner_member_and_collaborator() {
        assert_eq!(ALLOWED_ASSOCIATIONS, &["OWNER", "MEMBER", "COLLABORATOR"]);
    }

    #[test]
    fn is_trusted_author_owner() {
        let issue = GitHubIssue {
            number: 1,
            id: 0,
            title: "t".to_string(),
            body: None,
            state: "open".to_string(),
            labels: vec![],
            user: GitHubUser {
                login: "u".to_string(),
            },
            created_at: String::new(),
            updated_at: String::new(),
            html_url: String::new(),
            node_id: None,
            pull_request: None,
            state_reason: None,
            author_association: Some("OWNER".to_string()),
        };
        assert!(is_trusted_author(&issue));
    }

    #[test]
    fn is_trusted_author_collaborator() {
        let issue = GitHubIssue {
            number: 1,
            id: 0,
            title: "t".to_string(),
            body: None,
            state: "open".to_string(),
            labels: vec![],
            user: GitHubUser {
                login: "u".to_string(),
            },
            created_at: String::new(),
            updated_at: String::new(),
            html_url: String::new(),
            node_id: None,
            pull_request: None,
            state_reason: None,
            author_association: Some("COLLABORATOR".to_string()),
        };
        assert!(is_trusted_author(&issue));
    }

    #[test]
    fn is_not_trusted_author_contributor() {
        let issue = GitHubIssue {
            number: 1,
            id: 0,
            title: "t".to_string(),
            body: None,
            state: "open".to_string(),
            labels: vec![],
            user: GitHubUser {
                login: "u".to_string(),
            },
            created_at: String::new(),
            updated_at: String::new(),
            html_url: String::new(),
            node_id: None,
            pull_request: None,
            state_reason: None,
            author_association: Some("CONTRIBUTOR".to_string()),
        };
        assert!(!is_trusted_author(&issue));
    }

    #[test]
    fn is_trusted_author_member() {
        let issue = GitHubIssue {
            number: 1,
            id: 0,
            title: "t".to_string(),
            body: None,
            state: "open".to_string(),
            labels: vec![],
            user: GitHubUser {
                login: "u".to_string(),
            },
            created_at: String::new(),
            updated_at: String::new(),
            html_url: String::new(),
            node_id: None,
            pull_request: None,
            state_reason: None,
            author_association: Some("MEMBER".to_string()),
        };
        assert!(is_trusted_author(&issue));
    }

    #[test]
    fn is_trusted_comment_author_direct() {
        use crate::github::types::GitHubComment;
        let c = GitHubComment {
            id: 1,
            body: "hi".to_string(),
            user: crate::github::types::GitHubUser {
                login: "u".to_string(),
            },
            created_at: String::new(),
            updated_at: None,
            html_url: None,
            issue_url: None,
            author_association: Some("OWNER".to_string()),
        };
        assert!(is_trusted_comment_author(&c));
    }

    #[test]
    fn is_trusted_comment_author_member() {
        use crate::github::types::GitHubComment;
        let c = GitHubComment {
            id: 1,
            body: "hi".to_string(),
            user: crate::github::types::GitHubUser {
                login: "u".to_string(),
            },
            created_at: String::new(),
            updated_at: None,
            html_url: None,
            issue_url: None,
            author_association: Some("MEMBER".to_string()),
        };
        assert!(is_trusted_comment_author(&c));
    }

    fn comment_json(id: u64, login: &str, association: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "body": format!("from {login}"),
            "user": { "login": login },
            "created_at": "2026-01-01T00:00:00Z",
            "issue_url": "https://api.github.com/repos/o/r/issues/1",
            "author_association": association,
        })
    }

    #[test]
    fn trusted_mentions_drops_contributor_on_owner_issue() {
        use crate::github::types::GitHubComment;
        let comment = GitHubComment {
            id: 42,
            body: "@orch please look".to_string(),
            user: GitHubUser {
                login: "stranger".to_string(),
            },
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: None,
            html_url: None,
            issue_url: Some("https://api.github.com/repos/o/r/issues/1".to_string()),
            author_association: Some("CONTRIBUTOR".to_string()),
        };
        let issue = GitHubIssue {
            number: 1,
            id: 0,
            title: "t".to_string(),
            body: None,
            state: "open".to_string(),
            labels: vec![],
            user: GitHubUser {
                login: "owner".to_string(),
            },
            created_at: String::new(),
            updated_at: String::new(),
            html_url: String::new(),
            node_id: None,
            pull_request: None,
            state_reason: None,
            author_association: Some("OWNER".to_string()),
        };
        assert!(is_trusted_author(&issue));
        assert!(!is_trusted_comment_author(&comment));
        let mentions = trusted_mentions([comment]);
        assert!(mentions.is_empty());
    }

    #[expect(
        clippy::await_holding_lock,
        reason = "the env lock stays held across the awaits so tests do not race on GH_TOKEN"
    )]
    #[tokio::test]
    async fn get_mentions_and_list_issue_comments_exclude_contributor_on_owner_issue() {
        use crate::github::http::GhHttp;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _env = crate::github::token::ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("GH_TOKEN");
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        unsafe { std::env::set_var("GH_TOKEN", "test_token") };

        let server = MockServer::start().await;
        let comments = serde_json::json!([
            comment_json(42, "stranger", "CONTRIBUTOR"),
            comment_json(43, "owner", "OWNER"),
        ]);
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(comments.clone()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/1/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(comments))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "number": 1,
                "id": 1,
                "title": "t",
                "state": "open",
                "labels": [],
                "user": { "login": "owner" },
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z",
                "html_url": "https://github.com/o/r/issues/1",
                "author_association": "OWNER",
            })))
            .expect(0)
            .mount(&server)
            .await;

        let backend = GitHubBackend::for_test("o/r", GhHttp::for_test(server.uri(), None));
        let mentions = backend
            .get_mentions("2026-01-01T00:00:00Z")
            .await
            .expect("get_mentions");
        let listed = backend
            .list_issue_comments(&ExternalId("1".to_string()))
            .await
            .expect("list_issue_comments");

        match prev {
            // SAFETY: nextest runs each test in its own process, so no other test touches the environment
            Some(value) => unsafe { std::env::set_var("GH_TOKEN", value) },
            // SAFETY: nextest runs each test in its own process, so no other test touches the environment
            None => unsafe { std::env::remove_var("GH_TOKEN") },
        }

        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].id, "43");
        assert_eq!(mentions[0].author, "owner");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "43");
        assert_eq!(listed[0].author, "owner");
    }

    fn make_test_issue(labels: &[&str]) -> GitHubIssue {
        GitHubIssue {
            number: 1,
            id: 0,
            title: "t".to_string(),
            body: None,
            state: "open".to_string(),
            labels: labels
                .iter()
                .map(|label| crate::github::types::GitHubLabel {
                    name: (*label).to_string(),
                    color: None,
                })
                .collect(),
            user: GitHubUser {
                login: "u".to_string(),
            },
            created_at: String::new(),
            updated_at: String::new(),
            html_url: String::new(),
            node_id: None,
            pull_request: None,
            state_reason: None,
            author_association: Some("OWNER".to_string()),
        }
    }

    /// Open issues carrying `status:done` must be excluded from active-open ingest.
    #[test]
    fn classify_active_open_issue_excludes_status_done() {
        assert!(classify_active_open_issue(make_test_issue(&["status:done"])).is_none());
        assert!(
            classify_active_open_issue(make_test_issue(&["status:routed", "status:done"]))
                .is_none()
        );
    }

    #[test]
    fn classify_active_open_issue_preserves_unlabeled_and_new_as_none_status() {
        let unlabeled = classify_active_open_issue(make_test_issue(&[]))
            .expect("unlabeled issue should still be ingested");
        assert_eq!(unlabeled.1, None);

        let status_new = classify_active_open_issue(make_test_issue(&["status:new"]))
            .expect("status:new issue should still be ingested");
        assert_eq!(status_new.1, None);
    }

    #[test]
    fn classify_active_open_issue_maps_active_status() {
        let routed = classify_active_open_issue(make_test_issue(&["status:routed"]))
            .expect("active status issue should be ingested");
        assert_eq!(routed.1, Some(Status::Routed));
    }
}
