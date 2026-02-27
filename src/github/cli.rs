//! `gh` CLI wrapper — structured args in, serde out.
//!
//! All GitHub API calls go through `gh api`. Auth is handled by `gh`.
//! We build the command args in Rust and deserialize the JSON output via serde.

use super::types::{GitHubComment, GitHubIssue, GitHubReview, GitHubReviewComment};
use tokio::process::Command;
use urlencoding;

pub struct GhCli;

impl GhCli {
    pub fn new() -> Self {
        Self
    }

    pub async fn graphql(&self, query: &str) -> anyhow::Result<serde_json::Value> {
        let output = Command::new("gh")
            .arg("api")
            .arg("graphql")
            .arg("-f")
            .arg(format!("query={}", urlencoding::encode(query)))
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api graphql failed: {stderr}");
        }

        Ok(serde_json::from_slice(&output.stdout)?)
    }

    /// Run `gh api` with args and return raw JSON bytes.
    async fn api(&self, args: &[&str]) -> anyhow::Result<Vec<u8>> {
        let output = Command::new("gh").arg("api").args(args).output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Ok(output.stdout)
    }

    /// Check `gh auth status`.
    pub async fn auth_status(&self) -> anyhow::Result<()> {
        let output = Command::new("gh").args(["auth", "status"]).output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh auth failed: {stderr}");
        }
        Ok(())
    }

    /// Create a GitHub issue.
    ///
    /// Uses `--input -` with a JSON payload for safe multiline body handling.
    pub async fn create_issue(
        &self,
        repo: &str,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> anyhow::Result<GitHubIssue> {
        let endpoint = format!("repos/{repo}/issues");
        let payload = serde_json::json!({
            "title": title,
            "body": body,
            "labels": labels,
        });
        let output = Command::new("gh")
            .arg("api")
            .args([&endpoint, "-X", "POST", "--input", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        // Write payload to stdin
        let mut child = output;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(payload.to_string().as_bytes()).await?;
            drop(stdin);
        }
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    /// Get a single issue.
    pub async fn get_issue(&self, repo: &str, number: &str) -> anyhow::Result<GitHubIssue> {
        let endpoint = format!("repos/{repo}/issues/{number}");
        let json = self.api(&[&endpoint]).await?;
        Ok(serde_json::from_slice(&json)?)
    }

    /// List issues filtered by a label.
    ///
    /// Uses `--paginate` with `--jq '.[]'` to fetch all pages and flatten
    /// the JSON arrays into a stream of objects that serde can parse.
    pub async fn list_issues(&self, repo: &str, label: &str) -> anyhow::Result<Vec<GitHubIssue>> {
        let endpoint = format!("repos/{repo}/issues");
        let labels_field = format!("labels={label}");
        let output = Command::new("gh")
            .arg("api")
            .arg("--paginate")
            .arg("--jq")
            .arg(".[]")
            .args([
                &endpoint,
                "-X",
                "GET",
                "-f",
                &labels_field,
                "-f",
                "state=open",
                "-f",
                "per_page=100",
            ])
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        // --jq '.[]' produces newline-delimited JSON objects (NDJSON)
        let stdout = String::from_utf8_lossy(&output.stdout);
        let items: Vec<GitHubIssue> = stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(items)
    }

    /// Add labels to an issue (appends to existing labels).
    ///
    /// Uses `--input -` with a JSON payload — the `-f labels[]=` form doesn't
    /// map correctly to the GitHub JSON API.
    #[allow(dead_code)] // available for backends, not all paths used yet
    pub async fn add_labels(
        &self,
        repo: &str,
        number: &str,
        labels: &[String],
    ) -> anyhow::Result<()> {
        let endpoint = format!("repos/{repo}/issues/{number}/labels");
        let payload = serde_json::json!({ "labels": labels });
        let mut child = Command::new("gh")
            .arg("api")
            .args([&endpoint, "-X", "POST", "--input", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(payload.to_string().as_bytes()).await?;
            drop(stdin);
        }
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Ok(())
    }

    /// Ensure a label exists on the repo, creating it if it does not.
    ///
    /// Mirrors the bash `_gh_ensure_label()`: GET the label; on 404 (or any
    /// error) POST to create it.  Failures on creation are tolerated — the
    /// caller is never blocked by a missing label (same as `|| true` in bash).
    pub async fn ensure_label(
        &self,
        repo: &str,
        name: &str,
        color: &str,
        description: &str,
    ) -> anyhow::Result<()> {
        let encoded = urlencoding::encode(name);
        let get_endpoint = format!("repos/{repo}/labels/{encoded}");

        // Check whether the label already exists.
        match self.api(&[&get_endpoint]).await {
            Ok(_) => return Ok(()), // label exists — nothing to do
            Err(e) if e.to_string().contains("404") => {
                tracing::debug!(repo, name, "ensure_label: label not found, creating");
            }
            Err(e) => {
                // Any other GET error — log and attempt creation anyway.
                tracing::warn!(repo, name, err = %e, "ensure_label: GET failed, attempting create");
            }
        }

        // Create the label.
        let create_endpoint = format!("repos/{repo}/labels");
        let payload = serde_json::json!({
            "name": name,
            "color": color,
            "description": description,
        });
        let mut child = Command::new("gh")
            .arg("api")
            .args([&create_endpoint, "-X", "POST", "--input", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(payload.to_string().as_bytes()).await?;
            drop(stdin);
        }
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // 422 means the label already exists (race condition) — not an error.
            if stderr.contains("422")
                || stderr.contains("already_exists")
                || stderr.contains("Unprocessable")
            {
                tracing::debug!(repo, name, "ensure_label: label already exists (race)");
                return Ok(());
            }
            // All other creation failures are tolerated (matches bash `|| true`).
            tracing::warn!(repo, name, stderr = %stderr, "ensure_label: create failed, continuing");
        }
        Ok(())
    }

    /// Remove a label from an issue.
    ///
    /// Returns Ok if the label was removed or didn't exist (404).
    /// Propagates all other errors to prevent silent state corruption.
    #[allow(dead_code)]
    pub async fn remove_label(&self, repo: &str, number: &str, label: &str) -> anyhow::Result<()> {
        let encoded = urlencoding::encode(label);
        let endpoint = format!("repos/{repo}/issues/{number}/labels/{encoded}");
        match self.api(&[&endpoint, "-X", "DELETE"]).await {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("404") => {
                tracing::debug!(repo, number, label, "label already removed (404)");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Add a comment to an issue.
    ///
    /// Uses `--input -` for safe multiline body handling.
    pub async fn add_comment(&self, repo: &str, number: &str, body: &str) -> anyhow::Result<()> {
        let endpoint = format!("repos/{repo}/issues/{number}/comments");
        let payload = serde_json::json!({ "body": body });
        let mut child = Command::new("gh")
            .arg("api")
            .args([&endpoint, "-X", "POST", "--input", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(payload.to_string().as_bytes()).await?;
            drop(stdin);
        }
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Ok(())
    }

    /// List comments on an issue.
    #[allow(dead_code)]
    pub async fn list_comments(
        &self,
        repo: &str,
        number: &str,
    ) -> anyhow::Result<Vec<GitHubComment>> {
        let endpoint = format!("repos/{repo}/issues/{number}/comments");
        let json = self.api(&[&endpoint]).await?;
        Ok(serde_json::from_slice(&json)?)
    }

    /// Get a PR's merged state by branch name.
    ///
    /// Returns Ok(true) if merged, Ok(false) if not merged or not a PR.
    pub async fn is_pr_merged(&self, repo: &str, branch: &str) -> anyhow::Result<bool> {
        // The GitHub PRs endpoint `head` filter requires "owner:branch" format
        let owner = repo
            .split('/')
            .next()
            .ok_or_else(|| anyhow::anyhow!("invalid repo format: {}", repo))?;
        let head = format!("{}:{}", owner, branch);

        // Search for closed PRs with this branch as head
        let endpoint = format!("repos/{repo}/pulls");
        let json = self
            .api(&[
                &endpoint,
                "-f",
                &format!("head={}", head),
                "-f",
                "state=closed",
                "-f",
                "per_page=1",
            ])
            .await?;

        // Parse the response - if we get an empty array, there's no PR for this branch
        let prs: Vec<serde_json::Value> = serde_json::from_slice(&json)?;
        if prs.is_empty() {
            return Ok(false);
        }

        // Check if the PR is merged
        let first_pr = &prs[0];
        let merged_at = first_pr.get("merged_at");
        Ok(merged_at.map(|v| !v.is_null()).unwrap_or(false))
    }

    /// Get issue/PR comments since a given timestamp.
    ///
    /// Uses `--paginate` with `--jq '.[]'` to fetch all pages and flatten
    /// the JSON arrays into a stream of objects that serde can parse.
    pub async fn get_mentions(
        &self,
        repo: &str,
        since: &str,
    ) -> anyhow::Result<Vec<GitHubComment>> {
        let endpoint = format!("repos/{repo}/issues/comments");
        let since_field = format!("since={}", since);
        let output = Command::new("gh")
            .arg("api")
            .arg("--paginate")
            .arg("--jq")
            .arg(".[]")
            .args([
                &endpoint,
                "-X",
                "GET",
                "-f",
                &since_field,
                "-f",
                "per_page=100",
            ])
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        // --jq '.[]' produces newline-delimited JSON objects (NDJSON)
        let stdout = String::from_utf8_lossy(&output.stdout);
        let items: Vec<GitHubComment> = stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(items)
    }

    /// Get the current authenticated username.
    pub async fn get_whoami(&self) -> anyhow::Result<String> {
        let json = self.api(&["user"]).await?;
        let user: serde_json::Value = serde_json::from_slice(&json)?;
        user.get("login")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("failed to get current user"))
    }

    /// Get PR number by branch name.
    ///
    /// Returns the PR number if an open PR exists for the branch, None otherwise.
    pub async fn get_pr_number(&self, repo: &str, branch: &str) -> anyhow::Result<Option<u64>> {
        let owner = repo
            .split('/')
            .next()
            .ok_or_else(|| anyhow::anyhow!("invalid repo format: {}", repo))?;
        let head = format!("{}:{}", owner, branch);

        let endpoint = format!("repos/{repo}/pulls");
        let json = self
            .api(&[
                &endpoint,
                "-f",
                &format!("head={}", head),
                "-f",
                "state=open",
                "-f",
                "per_page=1",
            ])
            .await?;

        let prs: Vec<serde_json::Value> = serde_json::from_slice(&json)?;
        if prs.is_empty() {
            return Ok(None);
        }

        prs[0]
            .get("number")
            .and_then(|n| n.as_u64())
            .ok_or_else(|| anyhow::anyhow!("PR missing number field"))
            .map(Some)
    }

    /// Get reviews for a PR.
    ///
    /// Uses `gh api` to fetch all reviews for a given PR number.
    pub async fn get_pr_reviews(
        &self,
        repo: &str,
        pr_number: u64,
    ) -> anyhow::Result<Vec<GitHubReview>> {
        let endpoint = format!("repos/{repo}/pulls/{pr_number}/reviews");
        let json = self.api(&[&endpoint]).await?;
        Ok(serde_json::from_slice(&json)?)
    }

    /// Get review comments for a PR review.
    ///
    /// Uses `gh api` to fetch all comments for a specific review.
    #[allow(dead_code)]
    pub async fn get_pr_review_comments(
        &self,
        repo: &str,
        pr_number: u64,
        review_id: u64,
    ) -> anyhow::Result<Vec<GitHubReviewComment>> {
        let endpoint = format!("repos/{repo}/pulls/{pr_number}/reviews/{review_id}/comments");
        let json = self.api(&[&endpoint]).await?;
        Ok(serde_json::from_slice(&json)?)
    }

    /// Get all review comments for a PR.
    ///
    /// Uses `gh api` to fetch all review comments (across all reviews) for a PR.
    pub async fn get_pr_comments(
        &self,
        repo: &str,
        pr_number: u64,
    ) -> anyhow::Result<Vec<GitHubReviewComment>> {
        let endpoint = format!("repos/{repo}/pulls/{pr_number}/comments");
        let json = self.api(&[&endpoint]).await?;
        Ok(serde_json::from_slice(&json)?)
    }

    /// Close a GitHub issue.
    pub async fn close_issue(&self, repo: &str, number: &str) -> anyhow::Result<()> {
        let endpoint = format!("repos/{repo}/issues/{number}");
        let payload = serde_json::json!({ "state": "closed" });
        let mut child = Command::new("gh")
            .arg("api")
            .args([&endpoint, "-X", "PATCH", "--input", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(payload.to_string().as_bytes()).await?;
            drop(stdin);
        }
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Ok(())
    }

    /// Check if a user is a collaborator on a repo.
    ///
    /// Returns true if the user has collaborator access, false otherwise.
    pub async fn is_collaborator(&self, repo: &str, username: &str) -> anyhow::Result<bool> {
        let endpoint = format!("repos/{repo}/collaborators/{username}");
        let output = Command::new("gh")
            .arg("api")
            .args([&endpoint, "-X", "GET"])
            .output()
            .await?;
        // 204 = is collaborator, 404 = not a collaborator
        if output.status.success() {
            Ok(true)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("404") {
                Ok(false)
            } else {
                anyhow::bail!("gh api failed: {stderr}");
            }
        }
    }

    pub async fn get_sub_issues(&self, repo: &str, number: &str) -> anyhow::Result<Vec<u64>> {
        // Parse owner and repo from "owner/repo" format
        let parts: Vec<&str> = repo.split('/').collect();
        if parts.len() != 2 {
            anyhow::bail!("invalid repo format: expected 'owner/repo', got '{}'", repo);
        }
        let (owner, repo_name) = (parts[0], parts[1]);

        let mut all_numbers: Vec<u64> = Vec::new();
        let mut cursor: Option<String> = None;
        let page_size = 100;
        let max_pages = 50;
        let mut page_count = 0;

        // GraphQL query to get sub-issues with pagination cursor
        loop {
            page_count += 1;
            if page_count > max_pages {
                tracing::warn!(
                    repo,
                    number,
                    "get_sub_issues hit max page limit ({max_pages})"
                );
                break;
            }
            let query = if let Some(ref after_cursor) = cursor {
                format!(
                    r#"{{
                        repository(owner: "{}", name: "{}") {{
                            issue(number: {}) {{
                                subIssues(first: {}, after: "{}") {{
                                    nodes {{
                                        number
                                    }}
                                    pageInfo {{
                                        hasNextPage
                                        endCursor
                                    }}
                                }}
                            }}
                        }}
                    }}"#,
                    owner, repo_name, number, page_size, after_cursor
                )
            } else {
                format!(
                    r#"{{
                        repository(owner: "{}", name: "{}") {{
                            issue(number: {}) {{
                                subIssues(first: {}) {{
                                    nodes {{
                                        number
                                    }}
                                    pageInfo {{
                                        hasNextPage
                                        endCursor
                                    }}
                                }}
                            }}
                        }}
                    }}"#,
                    owner, repo_name, number, page_size
                )
            };

            let result = self.graphql(&query).await?;

            // Parse the response to extract sub-issue numbers
            let sub_issues = result
                .get("data")
                .and_then(|d| d.get("repository"))
                .and_then(|r| r.get("issue"))
                .and_then(|i| i.get("subIssues"))
                .and_then(|s| s.get("nodes"))
                .and_then(|n| n.as_array());

            match sub_issues {
                Some(nodes) => {
                    let numbers: Vec<u64> = nodes
                        .iter()
                        .filter_map(|n| n.get("number").and_then(|num| num.as_u64()))
                        .collect();
                    all_numbers.extend(numbers);
                }
                None => break, // No sub-issues or issue not found
            }

            // Check if there are more pages
            let has_next = result
                .get("data")
                .and_then(|d| d.get("repository"))
                .and_then(|r| r.get("issue"))
                .and_then(|i| i.get("subIssues"))
                .and_then(|s| s.get("pageInfo"))
                .and_then(|p| p.get("hasNextPage"))
                .and_then(|h| h.as_bool())
                .unwrap_or(false);

            if !has_next {
                break;
            }

            // Get the cursor for the next page
            cursor = result
                .get("data")
                .and_then(|d| d.get("repository"))
                .and_then(|r| r.get("issue"))
                .and_then(|i| i.get("subIssues"))
                .and_then(|s| s.get("pageInfo"))
                .and_then(|p| p.get("endCursor"))
                .and_then(|c| c.as_str())
                .map(|s| s.to_string());
        }

        Ok(all_numbers)
    }
}

/// Return the hex color string for a given `status:*` label.
///
/// Matches the bash `_gh_status_color()` palette so labels created from Rust
/// look the same as those created by the shell orchestrator.
pub fn status_label_color(label: &str) -> &'static str {
    match label {
        "status:new" => "0e8a16",
        "status:routed" => "1d76db",
        "status:in_progress" => "fbca04",
        "status:done" => "6f42c1",
        "status:blocked" => "d73a4a",
        "status:in_review" => "0075ca",
        "status:needs_review" => "e4e669",
        _ => "c5def5",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_label_colors_match_bash_palette() {
        assert_eq!(status_label_color("status:new"), "0e8a16");
        assert_eq!(status_label_color("status:routed"), "1d76db");
        assert_eq!(status_label_color("status:in_progress"), "fbca04");
        assert_eq!(status_label_color("status:done"), "6f42c1");
        assert_eq!(status_label_color("status:blocked"), "d73a4a");
        assert_eq!(status_label_color("status:in_review"), "0075ca");
        assert_eq!(status_label_color("status:needs_review"), "e4e669");
    }

    #[test]
    fn unknown_label_gets_default_color() {
        assert_eq!(status_label_color("agent:claude"), "c5def5");
        assert_eq!(status_label_color("enhancement"), "c5def5");
        assert_eq!(status_label_color(""), "c5def5");
    }
}
