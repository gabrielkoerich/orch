//! `gh` CLI wrapper — structured args in, serde out (legacy fallback).
//!
//! Superseded by `http.rs` (native reqwest client with connection pooling).
//! Kept as a fallback; all production callers have migrated to `GhHttp`.
#![allow(dead_code)]

use super::backoff::{self, GhBackoff};
use super::types::{GitHubComment, GitHubIssue, GitHubReview, GitHubReviewComment};
use crate::cmd::CommandErrorContext;
use std::sync::Mutex;
use tokio::process::Command;
use urlencoding;

/// Global backoff state shared across all `GhCli` instances.
///
/// `GhCli::new()` is called in many places (backend, engine, projects, commands),
/// so the backoff must be process-global to protect the single GitHub token.
static BACKOFF: std::sync::LazyLock<Mutex<GhBackoff>> =
    std::sync::LazyLock::new(|| Mutex::new(GhBackoff::new()));

/// Parse NDJSON (newline-delimited JSON) output from `gh api --jq '.[]'`.
fn parse_ndjson<T: serde::de::DeserializeOwned>(stdout: &[u8]) -> anyhow::Result<Vec<T>> {
    let text = String::from_utf8_lossy(stdout);
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub struct GhCli {
    /// Resolved absolute path to the `gh` binary (e.g. `/opt/homebrew/bin/gh`).
    /// Needed because launchd services have a minimal PATH that excludes Homebrew.
    gh_path: std::path::PathBuf,
}

impl GhCli {
    pub fn new() -> Self {
        let gh_path = which::which("gh").unwrap_or_else(|_| {
            // launchd services have a minimal PATH (/usr/bin:/bin:/usr/sbin:/sbin)
            // that excludes Homebrew — try common install locations as fallback.
            let candidates = [
                "/opt/homebrew/bin/gh", // macOS ARM
                "/usr/local/bin/gh",    // macOS Intel / Linux
            ];
            candidates
                .iter()
                .map(std::path::PathBuf::from)
                .find(|p| p.exists())
                .unwrap_or_else(|| std::path::PathBuf::from("gh"))
        });
        Self { gh_path }
    }

    fn cmd(&self) -> Command {
        Command::new(&self.gh_path)
    }

    /// Check if the GitHub API is currently rate-limited.
    ///
    /// Returns the remaining backoff duration if active, None otherwise.
    /// Used by the engine loop to skip ticks entirely during backoff.
    pub fn is_rate_limited() -> Option<std::time::Duration> {
        BACKOFF.lock().ok().and_then(|b| b.is_active())
    }

    /// Return an error if backoff is active. Called at the top of every API method.
    fn check_backoff() -> anyhow::Result<()> {
        if let Some(remaining) = Self::is_rate_limited() {
            anyhow::bail!(
                "GitHub API rate-limited, backoff active for {}s",
                remaining.as_secs()
            );
        }
        Ok(())
    }

    /// Check stderr for rate-limit signals and record if found.
    fn maybe_record_rate_limit(stderr: &str) {
        if backoff::is_rate_limit_error(stderr) {
            if let Ok(mut b) = BACKOFF.lock() {
                b.record_rate_limit();
            }
        }
    }

    /// Record a successful API call.
    fn record_api_success() {
        if let Ok(mut b) = BACKOFF.lock() {
            b.record_success();
        }
    }

    pub async fn graphql(&self, query: &str) -> anyhow::Result<serde_json::Value> {
        self.graphql_with_headers(query, &[]).await
    }

    /// Run a GraphQL query with optional extra HTTP headers.
    pub async fn graphql_with_headers(
        &self,
        query: &str,
        headers: &[&str],
    ) -> anyhow::Result<serde_json::Value> {
        Self::check_backoff()?;

        let mut cmd = self.cmd();
        cmd.arg("api").arg("graphql");

        for header in headers {
            cmd.arg("-H").arg(header);
        }

        cmd.arg("-f")
            .arg(format!("query={}", urlencoding::encode(query)));

        let output = cmd.output_with_context().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Self::maybe_record_rate_limit(&stderr);
            anyhow::bail!("gh api graphql failed: {stderr}");
        }

        Self::record_api_success();
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    /// Run `gh api` with args and return raw JSON bytes.
    async fn api(&self, args: &[&str]) -> anyhow::Result<Vec<u8>> {
        Self::check_backoff()?;

        let output = self
            .cmd()
            .arg("api")
            .args(args)
            .output_with_context()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Self::maybe_record_rate_limit(&stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Self::record_api_success();
        Ok(output.stdout)
    }

    /// Check `gh auth status`.
    pub async fn auth_status(&self) -> anyhow::Result<()> {
        let output = self
            .cmd()
            .args(["auth", "status"])
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to execute `{}`: {e}", self.gh_path.display()))?;
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
        Self::check_backoff()?;
        let endpoint = format!("repos/{repo}/issues");
        let payload = serde_json::json!({
            "title": title,
            "body": body,
            "labels": labels,
        });
        let output = self
            .cmd()
            .arg("api")
            .args([&endpoint, "-X", "POST", "--input", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn_with_context()?;
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
            Self::maybe_record_rate_limit(&stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Self::record_api_success();
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
        Self::check_backoff()?;
        let endpoint = format!("repos/{repo}/issues");
        let labels_field = format!("labels={label}");
        let output = self
            .cmd()
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
            .output_with_context()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Self::maybe_record_rate_limit(&stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Self::record_api_success();
        parse_ndjson(&output.stdout)
    }

    /// List all open issues (no label filter).
    pub async fn list_all_open_issues(&self, repo: &str) -> anyhow::Result<Vec<GitHubIssue>> {
        Self::check_backoff()?;
        let endpoint = format!("repos/{repo}/issues");
        let output = self
            .cmd()
            .arg("api")
            .arg("--paginate")
            .arg("--jq")
            .arg(".[]")
            .args([
                &endpoint,
                "-X",
                "GET",
                "-f",
                "state=open",
                "-f",
                "per_page=100",
            ])
            .output_with_context()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Self::maybe_record_rate_limit(&stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Self::record_api_success();
        parse_ndjson(&output.stdout)
    }

    /// Add labels to an issue (appends to existing labels).
    ///
    /// Uses `--input -` with a JSON payload — the `-f labels[]=` form doesn't
    /// map correctly to the GitHub JSON API.
    ///
    /// Kept as a public helper for backends and integration tests that need to
    /// programmatically append labels. Intentionally retained for API parity
    /// with the original shell helper.
    pub async fn add_labels(
        &self,
        repo: &str,
        number: &str,
        labels: &[String],
    ) -> anyhow::Result<()> {
        Self::check_backoff()?;
        let endpoint = format!("repos/{repo}/issues/{number}/labels");
        let payload = serde_json::json!({ "labels": labels });
        let mut child = self
            .cmd()
            .arg("api")
            .args([&endpoint, "-X", "POST", "--input", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn_with_context()?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(payload.to_string().as_bytes()).await?;
            drop(stdin);
        }
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Self::maybe_record_rate_limit(&stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Self::record_api_success();
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
        let mut child = self
            .cmd()
            .arg("api")
            .args([&create_endpoint, "-X", "POST", "--input", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn_with_context()?;
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
            // Rate-limit errors should still be recorded for backoff.
            Self::maybe_record_rate_limit(&stderr);
            // All other creation failures are tolerated (matches bash `|| true`).
            tracing::warn!(repo, name, stderr = %stderr, "ensure_label: create failed, continuing");
        } else {
            Self::record_api_success();
        }
        Ok(())
    }

    /// Remove a label from an issue.
    ///
    /// Returns Ok if the label was removed or didn't exist (404).
    /// Propagates all other errors to prevent silent state corruption.
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
        Self::check_backoff()?;
        let endpoint = format!("repos/{repo}/issues/{number}/comments");
        let payload = serde_json::json!({ "body": body });
        let mut child = self
            .cmd()
            .arg("api")
            .args([&endpoint, "-X", "POST", "--input", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn_with_context()?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(payload.to_string().as_bytes()).await?;
            drop(stdin);
        }
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Self::maybe_record_rate_limit(&stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Self::record_api_success();
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
        Self::check_backoff()?;
        let endpoint = format!("repos/{repo}/issues/comments");
        let since_field = format!("since={}", since);
        let output = self
            .cmd()
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
            .output_with_context()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Self::maybe_record_rate_limit(&stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Self::record_api_success();
        parse_ndjson(&output.stdout)
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
        Self::check_backoff()?;
        let endpoint = format!("repos/{repo}/issues/{number}");
        let payload = serde_json::json!({ "state": "closed" });
        let mut child = self
            .cmd()
            .arg("api")
            .args([&endpoint, "-X", "PATCH", "--input", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn_with_context()?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(payload.to_string().as_bytes()).await?;
            drop(stdin);
        }
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Self::maybe_record_rate_limit(&stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Self::record_api_success();
        Ok(())
    }

    /// Check if a user is a collaborator on a repo.
    ///
    /// Returns true if the user has collaborator access, false otherwise.
    pub async fn is_collaborator(&self, repo: &str, username: &str) -> anyhow::Result<bool> {
        Self::check_backoff()?;
        let endpoint = format!("repos/{repo}/collaborators/{username}");
        let output = self
            .cmd()
            .arg("api")
            .args([&endpoint, "-X", "GET"])
            .output_with_context()
            .await?;
        // 204 = is collaborator, 404 = not a collaborator
        if output.status.success() {
            Self::record_api_success();
            Ok(true)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("404") {
                Self::record_api_success();
                Ok(false)
            } else {
                Self::maybe_record_rate_limit(&stderr);
                anyhow::bail!("gh api failed: {stderr}");
            }
        }
    }

    /// Add a sub-issue relationship using the GitHub GraphQL API.
    ///
    /// Requires the `sub_issues` GraphQL preview feature header.
    pub async fn add_sub_issue(
        &self,
        parent_node_id: &str,
        child_node_id: &str,
    ) -> anyhow::Result<()> {
        let query = format!(
            r#"mutation {{
                addSubIssue(input: {{issueId: "{}", subIssueId: "{}"}}) {{
                    issue {{ number }}
                    subIssue {{ number }}
                }}
            }}"#,
            parent_node_id, child_node_id
        );

        self.graphql_with_headers(&query, &["GraphQL-Features:sub_issues"])
            .await?;
        Ok(())
    }

    /// Merge a PR using squash merge.
    ///
    /// Returns Ok(()) on success, or an error with stderr message on failure.
    pub async fn merge_pr(
        &self,
        repo: &str,
        pr_number: u64,
        delete_branch: bool,
    ) -> anyhow::Result<()> {
        Self::check_backoff()?;
        let mut args = vec![
            "pr".to_string(),
            "merge".to_string(),
            pr_number.to_string(),
            "--squash".to_string(),
            "--yes".to_string(),
            "--repo".to_string(),
            repo.to_string(),
        ];

        if delete_branch {
            args.push("--delete-branch".to_string());
        }

        let output = self.cmd().args(&args).output_with_context().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Self::maybe_record_rate_limit(&stderr);
            anyhow::bail!("gh pr merge failed: {}", stderr);
        }
        Self::record_api_success();
        Ok(())
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

            // Build query with optional cursor (single template, no duplication)
            let after_clause = cursor
                .as_ref()
                .map(|c| format!(r#", after: "{}""#, c))
                .unwrap_or_default();
            let query = format!(
                r#"{{
                    repository(owner: "{}", name: "{}") {{
                        issue(number: {}) {{
                            subIssues(first: {}{}) {{
                                nodes {{ number }}
                                pageInfo {{ hasNextPage endCursor }}
                            }}
                        }}
                    }}
                }}"#,
                owner, repo_name, number, page_size, after_clause
            );

            // Sub-issues API requires the preview feature header
            let result = self
                .graphql_with_headers(&query, &["GraphQL-Features:sub_issues"])
                .await?;

            // Extract subIssues object once, reuse for nodes and pageInfo
            let sub_issues_data = result
                .get("data")
                .and_then(|d| d.get("repository"))
                .and_then(|r| r.get("issue"))
                .and_then(|i| i.get("subIssues"));

            let nodes = sub_issues_data
                .and_then(|s| s.get("nodes"))
                .and_then(|n| n.as_array());

            match nodes {
                Some(nodes) => {
                    let numbers: Vec<u64> = nodes
                        .iter()
                        .filter_map(|n| n.get("number").and_then(|num| num.as_u64()))
                        .collect();
                    all_numbers.extend(numbers);
                }
                None => break,
            }

            let page_info = sub_issues_data.and_then(|s| s.get("pageInfo"));
            let has_next = page_info
                .and_then(|p| p.get("hasNextPage"))
                .and_then(|h| h.as_bool())
                .unwrap_or(false);

            if !has_next {
                break;
            }

            cursor = page_info
                .and_then(|p| p.get("endCursor"))
                .and_then(|c| c.as_str())
                .map(|s| s.to_string());
        }

        Ok(all_numbers)
    }

    /// Get the combined CI check status for a git ref (branch or SHA).
    ///
    /// Returns a tuple of `(state, total, passing, failing, pending)`.
    /// `state` is one of "success", "failure", "pending".
    pub async fn get_combined_status(
        &self,
        repo: &str,
        git_ref: &str,
    ) -> anyhow::Result<(String, u64, u64, u64, u64)> {
        Self::check_backoff()?;
        // Use the checks endpoint which combines both status checks and check runs
        let endpoint = format!("repos/{repo}/commits/{git_ref}/check-runs");
        let output = self
            .cmd()
            .arg("api")
            .arg("--paginate")
            .arg("--jq")
            .arg(".check_runs[]")
            .args([&endpoint])
            .output_with_context()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Self::maybe_record_rate_limit(&stderr);
            anyhow::bail!("gh api failed: {stderr}");
        }
        Self::record_api_success();

        let runs: Vec<serde_json::Value> = parse_ndjson(&output.stdout)?;

        let mut passing = 0u64;
        let mut failing = 0u64;
        let mut pending = 0u64;

        for run in &runs {
            let conclusion = run.get("conclusion").and_then(|v| v.as_str()).unwrap_or("");
            let status = run
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("queued");

            match status {
                "completed" => match conclusion {
                    "success" | "neutral" | "skipped" => passing += 1,
                    _ => failing += 1,
                },
                _ => pending += 1,
            }
        }

        let total = runs.len() as u64;
        let state = if failing > 0 {
            "failure".to_string()
        } else if pending > 0 || total == 0 {
            "pending".to_string()
        } else {
            "success".to_string()
        };

        Ok((state, total, passing, failing, pending))
    }

    /// Re-run failed jobs for a GitHub Actions workflow run.
    pub async fn rerun_failed_jobs(&self, repo: &str, run_id: u64) -> anyhow::Result<()> {
        Self::check_backoff()?;
        let endpoint = format!("repos/{repo}/actions/runs/{run_id}/rerun-failed-jobs");
        let output = self
            .cmd()
            .arg("api")
            .args([&endpoint, "-X", "POST"])
            .output_with_context()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Self::maybe_record_rate_limit(&stderr);
            anyhow::bail!("rerun-failed-jobs failed: {stderr}");
        }
        Self::record_api_success();
        Ok(())
    }

    /// Get the latest workflow run for a branch.
    ///
    /// Returns `(run_id, status, conclusion)` for the most recent run.
    pub async fn get_latest_run_for_branch(
        &self,
        repo: &str,
        branch: &str,
    ) -> anyhow::Result<Option<(u64, String, String)>> {
        let endpoint = format!("repos/{repo}/actions/runs");
        let json = self
            .api(&[
                &endpoint,
                "-f",
                &format!("branch={branch}"),
                "-f",
                "per_page=1",
            ])
            .await?;

        let resp: serde_json::Value = serde_json::from_slice(&json)?;
        let runs = resp
            .get("workflow_runs")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        if runs.is_empty() {
            return Ok(None);
        }

        let run = &runs[0];
        let id = run.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let status = run
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let conclusion = run
            .get("conclusion")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        Ok(Some((id, status, conclusion)))
    }

    /// Trigger a workflow dispatch for a specific branch.
    pub async fn dispatch_workflow(
        &self,
        repo: &str,
        workflow: &str,
        branch: &str,
    ) -> anyhow::Result<()> {
        Self::check_backoff()?;
        let endpoint = format!("repos/{repo}/actions/workflows/{workflow}/dispatches");
        let payload = serde_json::json!({ "ref": branch });
        let mut child = self
            .cmd()
            .arg("api")
            .args([&endpoint, "-X", "POST", "--input", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn_with_context()?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(payload.to_string().as_bytes()).await?;
            drop(stdin);
        }
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Self::maybe_record_rate_limit(&stderr);
            anyhow::bail!("workflow dispatch failed: {stderr}");
        }
        Self::record_api_success();
        Ok(())
    }

    /// Check the latest automated review comment on a PR.
    ///
    /// Returns `Some("approve")` or `Some("changes_requested")` based on the
    /// latest PR issue comment starting with "## Automated Review".
    /// Returns `None` if no automated review comment exists.
    ///
    /// Only considers comments from collaborators on the repo to prevent
    /// spoofing by unauthorized users.
    pub async fn get_automated_review_status(
        &self,
        repo: &str,
        pr_number: u64,
    ) -> anyhow::Result<Option<String>> {
        let comments = self.list_comments(repo, &pr_number.to_string()).await?;

        // Find the latest "Automated Review" comment from a collaborator
        let mut latest: Option<&crate::github::types::GitHubComment> = None;
        for c in comments.iter().rev() {
            if !c.body.starts_with("## Automated Review") {
                continue;
            }
            // Verify the comment author is a repo collaborator
            match self.is_collaborator(repo, &c.user.login).await {
                Ok(true) => {
                    latest = Some(c);
                    break;
                }
                Ok(false) => {
                    tracing::warn!(
                        user = %c.user.login,
                        pr_number,
                        "ignoring automated review comment from non-collaborator"
                    );
                    continue;
                }
                Err(e) => {
                    tracing::debug!(
                        user = %c.user.login,
                        error = %e,
                        "failed to check collaborator status, skipping comment"
                    );
                    continue;
                }
            }
        }

        match latest {
            Some(c) => {
                let first_line = c.body.lines().next().unwrap_or("");
                if first_line.contains("Automated Review \u{2014} Approve") {
                    Ok(Some("approve".to_string()))
                } else if first_line.contains("Automated Review \u{2014} Changes Requested") {
                    Ok(Some("changes_requested".to_string()))
                } else {
                    Ok(None)
                }
            }
            None => Ok(None),
        }
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

    #[test]
    fn parse_ndjson_empty_input() {
        let result: Vec<serde_json::Value> = parse_ndjson(b"").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn parse_ndjson_single_object() {
        let input = br#"{"id": 1, "name": "test"}"#;
        let result: Vec<serde_json::Value> = parse_ndjson(input).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], 1);
    }

    #[test]
    fn parse_ndjson_multiple_objects() {
        let input = b"{\"a\": 1}\n{\"a\": 2}\n{\"a\": 3}\n";
        let result: Vec<serde_json::Value> = parse_ndjson(input).unwrap();
        assert_eq!(result.len(), 3);
    }

    /// Test the review comment header parsing logic used by
    /// `get_automated_review_status` and the GitHub Actions workflow.
    #[test]
    fn review_comment_header_approve() {
        let body = "## Automated Review \u{2014} Approve\n\nLooks good!";
        let first_line = body.lines().next().unwrap_or("");
        assert!(first_line.contains("Automated Review \u{2014} Approve"));
        assert!(!first_line.contains("Changes Requested"));
    }

    #[test]
    fn review_comment_header_changes_requested() {
        let body = "## Automated Review \u{2014} Changes Requested\n\nPlease fix the issues below.";
        let first_line = body.lines().next().unwrap_or("");
        assert!(first_line.contains("Automated Review \u{2014} Changes Requested"));
        assert!(!first_line.contains("Approve\n"));
    }

    #[test]
    fn review_comment_header_unknown_format() {
        let body = "## Automated Review\n\nSome other format";
        let first_line = body.lines().next().unwrap_or("");
        assert!(!first_line.contains("Approve"));
        assert!(!first_line.contains("Changes Requested"));
    }

    #[test]
    fn review_comment_non_review_comment_ignored() {
        let body = "This is a regular comment, not a review.";
        assert!(!body.starts_with("## Automated Review"));
    }
}
