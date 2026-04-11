//! GitHub Issues backend — native HTTP via `reqwest` with connection pooling.
//!
//! Auth is resolved once at startup using the centralized [`TokenResolver`]
//! (`GH_TOKEN` → `GITHUB_TOKEN` environment variables by default).

use super::{ExternalBackend, ExternalId, ExternalTask, Mention, Status};
use crate::github::http::{status_label_color, GhHttp};
use crate::github::projects::ProjectSync;
use async_trait::async_trait;
use chrono::{Duration, Utc};

/// Author associations that are allowed to create tasks.
/// Only repository OWNER or COLLABORATOR authors are considered trusted for
/// ingestion. Other associations (members, collaborators, first-time
/// contributors, etc.) are ignored per the new sync policy.
/// Available values: COLLABORATOR, CONTRIBUTOR, FIRST_TIMER, FIRST_TIME_CONTRIBUTOR, MANNEQUIN, MEMBER, NONE, OWNER
const ALLOWED_ASSOCIATIONS: &[&str] = &["OWNER", "COLLABORATOR"];

/// Check if an issue author is trusted
fn is_trusted_author(issue: &crate::github::types::GitHubIssue) -> bool {
    issue
        .author_association
        .as_deref()
        .map(|a| ALLOWED_ASSOCIATIONS.contains(&a))
        .unwrap_or(false) // missing field = untrusted
}

// Check if a comment's author is trusted. The
// GitHub REST comment payload includes an `author_association` field, so
// prefer that when present. Missing association is treated as untrusted.
fn is_trusted_comment_author(c: &crate::github::types::GitHubComment) -> bool {
    c.author_association
        .as_deref()
        .map(|a| ALLOWED_ASSOCIATIONS.contains(&a))
        .unwrap_or(false)
}

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
        let project = match ProjectSync::from_config() {
            Some(p) => p,
            None => return Ok(None),
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
        project
            .sync_item_status(&node_id, &status)
            .await
            .map_err(|e| anyhow::anyhow!("project board sync failed: {e}"))?;

        Ok(Some(node_id))
    }

    async fn sync_estimate_to_project(&self, id: &ExternalId, estimate: u8) -> anyhow::Result<()> {
        let project = match ProjectSync::from_config() {
            Some(p) => p,
            None => return Ok(()),
        };

        // Only sync if the estimate field is configured.
        if project.estimate_field_id().is_none() {
            return Ok(());
        }

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
        let project = match ProjectSync::from_config() {
            Some(p) => p,
            None => return Ok(std::collections::HashMap::new()),
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
        // Internal tasks (e.g. "internal:63857") are not GitHub issues — skip silently.
        if !is_github_issue_id(id) {
            return Ok(());
        }

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

                    // Auto-close issue when status → Done (if auto_close enabled)
                    if status == Status::Done {
                        let auto_close = crate::config::get("workflow.auto_close")
                            .map(|v| v == "true")
                            .unwrap_or(true);
                        if auto_close {
                            if let Err(e) = self.gh.close_issue(&self.repo, &id.0).await {
                                tracing::warn!(
                                    task_id = id.0,
                                    err = %e,
                                    "auto-close issue failed (non-fatal)"
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
        let open = self.gh.list_all_open_issues(&self.repo).await?;
        let closed = self.gh.list_closed_issues_since(&self.repo, &since).await?;
        let issues = open.into_iter().chain(closed);

        Ok(issues
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

        let active_status_from_label = |label: &str| -> Option<crate::backends::Status> {
            match label {
                "status:routed" => Some(crate::backends::Status::Routed),
                "status:in_progress" => Some(crate::backends::Status::InProgress),
                "status:needs_review" => Some(crate::backends::Status::NeedsReview),
                "status:in_review" => Some(crate::backends::Status::InReview),
                "status:blocked" => Some(crate::backends::Status::Blocked),
                _ => None,
            }
        };

        Ok(issues
            .into_iter()
            .filter(|issue| issue.pull_request.is_none()) // Exclude PRs
            .filter(is_trusted_author) // Only trusted authors
            .map(|issue| {
                // Detect active (non-new) status from labels; None = unlabeled or status:new.
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
                (task, status)
            })
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
        ) {
            if let Some(parent_node_id) = parent_issue.node_id {
                if let Err(e) = self.gh.add_sub_issue(&parent_node_id, &child_node_id).await {
                    tracing::warn!(
                        parent = parent_id.0,
                        child = child_id.0,
                        err = %e,
                        "failed to add native sub-issue link (parent label still set)"
                    );
                }
            }
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
        // Only include mentions authored by trusted associations. The GitHub
        // REST issue comment payload does not include author_association, so
        // we conservatively check the parent issue's author_association when
        // an issue_url is available. If we cannot resolve association, skip
        // the comment.
        let mut out = Vec::new();
        for c in comments.into_iter() {
            // Prefer the comment's own author_association when present. If the
            // comment payload doesn't include it (older endpoints), fall back to
            // inspecting the parent issue's association when an issue_url is
            // available. If neither yields a trusted association, skip the
            // comment.
            let trusted = if is_trusted_comment_author(&c) {
                true
            } else if let Some(ref issue_url) = c.issue_url {
                // extract issue number and fetch issue to inspect author_association
                if let Some(n) = issue_url.rsplit('/').next() {
                    if let Ok(issue) = self.gh.get_issue(&self.repo, n).await {
                        is_trusted_author(&issue)
                    } else {
                        tracing::warn!(
                            repo = %self.repo,
                            issue_url = %issue_url,
                            comment_id = c.id,
                            author = %c.user.login,
                            "get_mentions: failed to fetch parent issue to check author trust, treating as untrusted — mention may be silently dropped"
                        );
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };

            if !trusted {
                tracing::debug!(comment_id = c.id, author = %c.user.login, "ignoring mention from untrusted author");
                continue;
            }

            out.push(Mention {
                id: c.id.to_string(),
                body: c.body,
                author: c.user.login,
                created_at: c.created_at,
                issue_url: c.issue_url,
            });
        }

        Ok(out)
    }

    async fn list_issue_comments(&self, id: &ExternalId) -> anyhow::Result<Vec<Mention>> {
        let issue_number: u64 =
            id.0.parse()
                .map_err(|_| anyhow::anyhow!("invalid issue number: {}", id.0))?;
        let comments = self.gh.get_issue_comments(&self.repo, issue_number).await?;
        // Filter comments to only include those from trusted associations.
        let mut out = Vec::new();
        for c in comments.into_iter() {
            // Prefer the comment's own association when present.
            if is_trusted_comment_author(&c) {
                out.push(Mention {
                    id: c.id.to_string(),
                    body: c.body,
                    author: c.user.login,
                    created_at: c.created_at,
                    issue_url: c.issue_url,
                });
                continue;
            }

            // Fall back to parent issue association when necessary.
            if let Ok(issue) = self.gh.get_issue(&self.repo, &id.0).await {
                if is_trusted_author(&issue) {
                    out.push(Mention {
                        id: c.id.to_string(),
                        body: c.body,
                        author: c.user.login,
                        created_at: c.created_at,
                        issue_url: c.issue_url,
                    });
                    continue;
                } else {
                    tracing::debug!(comment_id = c.id, author = %c.user.login, "ignoring issue comment from untrusted author");
                    continue;
                }
            } else {
                // If we cannot fetch the issue, skip the comment conservatively
                tracing::debug!(
                    comment_id = c.id,
                    "failed to fetch parent issue, skipping comment"
                );
                continue;
            }
        }

        Ok(out)
    }

    async fn acknowledge_issue(&self, id: &ExternalId) -> anyhow::Result<()> {
        self.gh.add_reaction(&self.repo, &id.0, "eyes").await
    }

    async fn acknowledge_mention(&self, comment_id: &str) -> anyhow::Result<()> {
        self.gh
            .add_comment_reaction(&self.repo, comment_id, "eyes")
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::types::{GitHubIssue, GitHubUser};

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
        assert!(!is_github_issue_id(&ExternalId("".to_string())));
        assert!(!is_github_issue_id(&ExternalId("abc".to_string())));
        assert!(!is_github_issue_id(&ExternalId("1.0".to_string())));
    }

    // ---

    #[test]
    fn allowed_associations_are_owner_and_contributor() {
        assert_eq!(ALLOWED_ASSOCIATIONS, &["OWNER", "COLLABORATOR"]);
    }

    #[test]
    fn is_trusted_author_owner() {
        let issue = GitHubIssue {
            number: 1,
            title: "t".to_string(),
            body: None,
            state: "open".to_string(),
            labels: vec![],
            user: GitHubUser {
                login: "u".to_string(),
            },
            created_at: "".to_string(),
            updated_at: "".to_string(),
            html_url: "".to_string(),
            node_id: None,
            pull_request: None,
            author_association: Some("OWNER".to_string()),
        };
        assert!(is_trusted_author(&issue));
    }

    #[test]
    fn is_trusted_author_collaborator() {
        let issue = GitHubIssue {
            number: 1,
            title: "t".to_string(),
            body: None,
            state: "open".to_string(),
            labels: vec![],
            user: GitHubUser {
                login: "u".to_string(),
            },
            created_at: "".to_string(),
            updated_at: "".to_string(),
            html_url: "".to_string(),
            node_id: None,
            pull_request: None,
            author_association: Some("COLLABORATOR".to_string()),
        };
        assert!(is_trusted_author(&issue));
    }

    #[test]
    fn is_not_trusted_author_contributor() {
        let issue = GitHubIssue {
            number: 1,
            title: "t".to_string(),
            body: None,
            state: "open".to_string(),
            labels: vec![],
            user: GitHubUser {
                login: "u".to_string(),
            },
            created_at: "".to_string(),
            updated_at: "".to_string(),
            html_url: "".to_string(),
            node_id: None,
            pull_request: None,
            author_association: Some("CONTRIBUTOR".to_string()),
        };
        assert!(!is_trusted_author(&issue));
    }

    #[test]
    fn is_not_trusted_author_member() {
        let issue = GitHubIssue {
            number: 1,
            title: "t".to_string(),
            body: None,
            state: "open".to_string(),
            labels: vec![],
            user: GitHubUser {
                login: "u".to_string(),
            },
            created_at: "".to_string(),
            updated_at: "".to_string(),
            html_url: "".to_string(),
            node_id: None,
            pull_request: None,
            author_association: Some("MEMBER".to_string()),
        };
        assert!(!is_trusted_author(&issue));
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
            created_at: "".to_string(),
            updated_at: None,
            html_url: None,
            issue_url: None,
            author_association: Some("OWNER".to_string()),
        };
        assert!(is_trusted_comment_author(&c));
    }

    #[test]
    fn is_not_trusted_comment_author_direct() {
        use crate::github::types::GitHubComment;
        let c = GitHubComment {
            id: 1,
            body: "hi".to_string(),
            user: crate::github::types::GitHubUser {
                login: "u".to_string(),
            },
            created_at: "".to_string(),
            updated_at: None,
            html_url: None,
            issue_url: None,
            author_association: Some("MEMBER".to_string()),
        };
        assert!(!is_trusted_comment_author(&c));
    }
}
