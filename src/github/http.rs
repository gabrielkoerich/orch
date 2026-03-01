//! Native `reqwest` HTTP client for the GitHub API — replaces `gh` CLI subprocesses.
//!
//! Uses a shared `reqwest::Client` with connection pooling, reads rate-limit
//! headers proactively, and supports concurrent requests via `tokio::join!`.
//!
//! Auth: reads token from `gh auth token` once at startup, falls back to
//! `GITHUB_TOKEN` / `GH_TOKEN` env vars.

use super::types::{GitHubComment, GitHubIssue, GitHubReview, GitHubReviewComment};
use reqwest::{header, Client, Response, StatusCode};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use urlencoding;

const GITHUB_API: &str = "https://api.github.com";

// ── Rate-limit state ─────────────────────────────────────────────────

/// Proactive rate-limit state derived from `X-RateLimit-*` response headers.
struct RateLimit {
    /// Remaining requests in the current window.
    remaining: Option<u32>,
    /// UTC epoch second when the window resets.
    reset_at: Option<u64>,
    /// Exponential backoff after a 403 (fallback when headers are absent).
    backoff_until: Option<Instant>,
    backoff_delay: Duration,
    backoff_base: Duration,
    backoff_max: Duration,
}

impl RateLimit {
    fn new() -> Self {
        let base = crate::config::get("gh.backoff.base_seconds")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30);
        let max = crate::config::get("gh.backoff.max_seconds")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(900);

        Self {
            remaining: None,
            reset_at: None,
            backoff_until: None,
            backoff_delay: Duration::ZERO,
            backoff_base: Duration::from_secs(base),
            backoff_max: Duration::from_secs(max),
        }
    }

    /// Update state from response headers.
    fn update_from_headers(&mut self, headers: &header::HeaderMap) {
        if let Some(v) = headers.get("x-ratelimit-remaining") {
            self.remaining = v.to_str().ok().and_then(|s| s.parse().ok());
        }
        if let Some(v) = headers.get("x-ratelimit-reset") {
            self.reset_at = v.to_str().ok().and_then(|s| s.parse().ok());
        }
    }

    /// Record a successful call — reset backoff.
    fn record_success(&mut self) {
        if self.backoff_delay > Duration::ZERO {
            tracing::info!("GitHub backoff cleared after successful API call");
        }
        self.backoff_delay = Duration::ZERO;
        self.backoff_until = None;
    }

    /// Record a 403 — escalate exponential backoff.
    fn record_rate_limit(&mut self) {
        self.backoff_delay = if self.backoff_delay.is_zero() {
            self.backoff_base
        } else {
            (self.backoff_delay * 2).min(self.backoff_max)
        };
        self.backoff_until = Some(Instant::now() + self.backoff_delay);
        tracing::warn!(
            delay_secs = self.backoff_delay.as_secs(),
            "GitHub rate limit hit, backing off"
        );
    }

    /// Returns remaining backoff/pause duration, or None if free to proceed.
    fn is_active(&self) -> Option<Duration> {
        // 1. Check exponential backoff (403-triggered)
        if let Some(until) = self.backoff_until {
            let now = Instant::now();
            if now < until {
                return Some(until - now);
            }
        }
        // 2. Proactive pause: if remaining == 0, wait until reset
        if self.remaining == Some(0) {
            if let Some(reset_epoch) = self.reset_at {
                let now_epoch = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                if now_epoch < reset_epoch {
                    return Some(Duration::from_secs(reset_epoch - now_epoch));
                }
            }
        }
        None
    }
}

// ── Global state ─────────────────────────────────────────────────────

static RATE_LIMIT: std::sync::LazyLock<Mutex<RateLimit>> =
    std::sync::LazyLock::new(|| Mutex::new(RateLimit::new()));

// ── GhHttp client ────────────────────────────────────────────────────

/// Native HTTP client for the GitHub API with connection pooling and
/// proactive rate-limit avoidance.
#[derive(Clone)]
pub struct GhHttp {
    client: Client,
    token: String,
}

impl GhHttp {
    /// Create a new client. Reads the GitHub token once (env var → `gh auth token`).
    pub fn new() -> Self {
        let token = resolve_token();
        let client = Client::builder()
            .user_agent("orch/0.1 (reqwest)")
            .pool_max_idle_per_host(4)
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");

        Self { client, token }
    }

    // ── Rate-limit helpers (static, like the old GhCli) ──────────

    /// Check if the GitHub API is currently rate-limited.
    pub fn is_rate_limited() -> Option<Duration> {
        RATE_LIMIT.lock().ok().and_then(|rl| rl.is_active())
    }

    fn check_backoff() -> anyhow::Result<()> {
        if let Some(remaining) = Self::is_rate_limited() {
            anyhow::bail!(
                "GitHub API rate-limited, backoff active for {}s",
                remaining.as_secs()
            );
        }
        Ok(())
    }

    fn record_response(resp: &Response) {
        if let Ok(mut rl) = RATE_LIMIT.lock() {
            rl.update_from_headers(resp.headers());
            // Only backoff on 429 (always rate limit) — 403 requires body inspection
            // which is handled in maybe_record_rate_limit_from_body().
            if resp.status() == StatusCode::TOO_MANY_REQUESTS {
                rl.record_rate_limit();
            } else if resp.status().is_success() {
                rl.record_success();
            }
        }
    }

    /// Check response body for rate-limit signals on 403 responses.
    /// Not all 403s are rate limits — some are permission errors.
    fn maybe_record_rate_limit_from_body(status: StatusCode, body: &str) {
        if status == StatusCode::FORBIDDEN {
            let lower = body.to_lowercase();
            if lower.contains("rate limit")
                || lower.contains("abuse detection")
                || lower.contains("secondary rate")
            {
                if let Ok(mut rl) = RATE_LIMIT.lock() {
                    rl.record_rate_limit();
                }
            }
        }
    }

    fn record_success() {
        if let Ok(mut rl) = RATE_LIMIT.lock() {
            rl.record_success();
        }
    }

    // ── Low-level HTTP helpers ────────────────────────────────────

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// GET request, returns deserialized JSON.
    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> anyhow::Result<T> {
        Self::check_backoff()?;
        let resp = self
            .client
            .get(url)
            .header(header::AUTHORIZATION, self.auth_header())
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;
        Self::record_response(&resp);
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            Self::maybe_record_rate_limit_from_body(status, &body);
            anyhow::bail!("GitHub API GET {url} failed ({status}): {body}");
        }
        Ok(serde_json::from_str(&resp.text().await?)?)
    }

    /// GET raw bytes (for endpoints that return non-JSON or we parse manually).
    async fn get_bytes(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        Self::check_backoff()?;
        let resp = self
            .client
            .get(url)
            .header(header::AUTHORIZATION, self.auth_header())
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;
        Self::record_response(&resp);
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            Self::maybe_record_rate_limit_from_body(status, &body);
            anyhow::bail!("GitHub API GET {url} failed ({status}): {body}");
        }
        Ok(resp.bytes().await?.to_vec())
    }

    /// GET with query params, returns deserialized JSON.
    async fn get_with_query<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        query: &[(&str, &str)],
    ) -> anyhow::Result<T> {
        Self::check_backoff()?;
        let resp = self
            .client
            .get(url)
            .query(query)
            .header(header::AUTHORIZATION, self.auth_header())
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;
        Self::record_response(&resp);
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            Self::maybe_record_rate_limit_from_body(status, &body);
            anyhow::bail!("GitHub API GET {url} failed ({status}): {body}");
        }
        Ok(serde_json::from_str(&resp.text().await?)?)
    }

    /// POST with JSON body, returns raw response text.
    async fn post_json_raw(&self, url: &str, body: &serde_json::Value) -> anyhow::Result<String> {
        Self::check_backoff()?;
        let resp = self
            .client
            .post(url)
            .json(body)
            .header(header::AUTHORIZATION, self.auth_header())
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;
        Self::record_response(&resp);
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            Self::maybe_record_rate_limit_from_body(status, &text);
            anyhow::bail!("GitHub API POST {url} failed ({status}): {text}");
        }
        Ok(text)
    }

    /// POST with JSON body, returns deserialized JSON.
    async fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<T> {
        let text = self.post_json_raw(url, body).await?;
        Ok(serde_json::from_str(&text)?)
    }

    /// PATCH with JSON body, returns raw response.
    async fn patch_json_raw(&self, url: &str, body: &serde_json::Value) -> anyhow::Result<String> {
        Self::check_backoff()?;
        let resp = self
            .client
            .patch(url)
            .json(body)
            .header(header::AUTHORIZATION, self.auth_header())
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;
        Self::record_response(&resp);
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            Self::maybe_record_rate_limit_from_body(status, &text);
            anyhow::bail!("GitHub API PATCH {url} failed ({status}): {text}");
        }
        Ok(text)
    }

    /// DELETE request.
    async fn delete(&self, url: &str) -> anyhow::Result<StatusCode> {
        Self::check_backoff()?;
        let resp = self
            .client
            .delete(url)
            .header(header::AUTHORIZATION, self.auth_header())
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;
        Self::record_response(&resp);
        let status = resp.status();
        if !status.is_success() && status != StatusCode::NOT_FOUND {
            let body = resp.text().await.unwrap_or_default();
            Self::maybe_record_rate_limit_from_body(status, &body);
            anyhow::bail!("GitHub API DELETE {url} failed ({status}): {body}");
        }
        Ok(status)
    }

    /// Paginated GET — follows Link: <next> headers.
    async fn get_all_pages<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        query: &[(&str, &str)],
    ) -> anyhow::Result<Vec<T>> {
        Self::check_backoff()?;
        let mut all: Vec<T> = Vec::new();
        let mut next_url: Option<String> = None;
        let mut is_first = true;

        loop {
            let resp = if is_first {
                is_first = false;
                self.client
                    .get(url)
                    .query(query)
                    .header(header::AUTHORIZATION, self.auth_header())
                    .header(header::ACCEPT, "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28")
                    .send()
                    .await?
            } else {
                let u = next_url.as_ref().unwrap();
                self.client
                    .get(u)
                    .header(header::AUTHORIZATION, self.auth_header())
                    .header(header::ACCEPT, "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28")
                    .send()
                    .await?
            };
            Self::record_response(&resp);
            let status = resp.status();

            // Parse Link header for next page
            next_url = parse_link_next(resp.headers());

            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                Self::maybe_record_rate_limit_from_body(status, &body);
                anyhow::bail!("GitHub API GET (paginated) failed ({status}): {body}");
            }

            let text = resp.text().await?;
            let page: Vec<T> = serde_json::from_str(&text)?;
            all.extend(page);

            if next_url.is_none() {
                break;
            }
        }

        Ok(all)
    }

    /// POST to the GraphQL endpoint.
    async fn graphql_request(
        &self,
        query: &str,
        extra_headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        Self::check_backoff()?;
        let body = serde_json::json!({ "query": query });
        let mut req = self
            .client
            .post(format!("{GITHUB_API}/graphql"))
            .json(&body)
            .header(header::AUTHORIZATION, self.auth_header())
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");

        for (k, v) in extra_headers {
            req = req.header(*k, *v);
        }

        let resp = req.send().await?;
        Self::record_response(&resp);
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            Self::maybe_record_rate_limit_from_body(status, &text);
            anyhow::bail!("GitHub GraphQL failed ({status}): {text}");
        }
        Self::record_success();
        Ok(serde_json::from_str(&text)?)
    }

    // ── Public API (mirrors GhCli) ───────────────────────────────

    pub async fn graphql(&self, query: &str) -> anyhow::Result<serde_json::Value> {
        self.graphql_request(query, &[]).await
    }

    pub async fn graphql_with_headers(
        &self,
        query: &str,
        headers: &[&str],
    ) -> anyhow::Result<serde_json::Value> {
        // Convert "Key:Value" strings to (&str, &str) tuples.
        let pairs: Vec<(&str, &str)> = headers.iter().filter_map(|h| h.split_once(':')).collect();
        self.graphql_request(query, &pairs).await
    }

    /// Verify authentication by fetching the current user.
    pub async fn auth_status(&self) -> anyhow::Result<()> {
        let _: serde_json::Value = self.get_json(&format!("{GITHUB_API}/user")).await?;
        Ok(())
    }

    /// Create a GitHub issue.
    pub async fn create_issue(
        &self,
        repo: &str,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> anyhow::Result<GitHubIssue> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues");
        let payload = serde_json::json!({
            "title": title,
            "body": body,
            "labels": labels,
        });
        self.post_json(&url, &payload).await
    }

    /// Get a single issue.
    pub async fn get_issue(&self, repo: &str, number: &str) -> anyhow::Result<GitHubIssue> {
        self.get_json(&format!("{GITHUB_API}/repos/{repo}/issues/{number}"))
            .await
    }

    /// List issues filtered by a label (paginated).
    pub async fn list_issues(&self, repo: &str, label: &str) -> anyhow::Result<Vec<GitHubIssue>> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues");
        let all: Vec<GitHubIssue> = self
            .get_all_pages(
                &url,
                &[("labels", label), ("state", "open"), ("per_page", "100")],
            )
            .await?;
        // GitHub /issues API returns PRs too — filter them out
        Ok(all
            .into_iter()
            .filter(|i| i.pull_request.is_none())
            .collect())
    }

    /// List all open issues (no label filter, paginated).
    pub async fn list_all_open_issues(&self, repo: &str) -> anyhow::Result<Vec<GitHubIssue>> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues");
        let all: Vec<GitHubIssue> = self
            .get_all_pages(&url, &[("state", "open"), ("per_page", "100")])
            .await?;
        // GitHub /issues API returns PRs too — filter them out
        Ok(all
            .into_iter()
            .filter(|i| i.pull_request.is_none())
            .collect())
    }

    /// Add labels to an issue.
    pub async fn add_labels(
        &self,
        repo: &str,
        number: &str,
        labels: &[String],
    ) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues/{number}/labels");
        let payload = serde_json::json!({ "labels": labels });
        self.post_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Ensure a label exists on the repo, creating it if needed.
    pub async fn ensure_label(
        &self,
        repo: &str,
        name: &str,
        color: &str,
        description: &str,
    ) -> anyhow::Result<()> {
        let encoded = urlencoding::encode(name);
        let get_url = format!("{GITHUB_API}/repos/{repo}/labels/{encoded}");

        // Check if label exists
        match self.get_bytes(&get_url).await {
            Ok(_) => return Ok(()),
            Err(e) if e.to_string().contains("404") => {
                tracing::debug!(repo, name, "ensure_label: label not found, creating");
            }
            Err(e) => {
                tracing::warn!(repo, name, err = %e, "ensure_label: GET failed, attempting create");
            }
        }

        // Create the label
        let create_url = format!("{GITHUB_API}/repos/{repo}/labels");
        let payload = serde_json::json!({
            "name": name,
            "color": color,
            "description": description,
        });

        match self.post_json_raw(&create_url, &payload).await {
            Ok(_) => {
                Self::record_success();
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("422")
                    || msg.contains("already_exists")
                    || msg.contains("Unprocessable")
                {
                    tracing::debug!(repo, name, "ensure_label: label already exists (race)");
                    return Ok(());
                }
                // Tolerate creation failures (matches old behavior).
                tracing::warn!(repo, name, err = %msg, "ensure_label: create failed, continuing");
            }
        }
        Ok(())
    }

    /// Remove a label from an issue.
    pub async fn remove_label(&self, repo: &str, number: &str, label: &str) -> anyhow::Result<()> {
        let encoded = urlencoding::encode(label);
        let url = format!("{GITHUB_API}/repos/{repo}/issues/{number}/labels/{encoded}");
        match self.delete(&url).await {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("404") => {
                tracing::debug!(repo, number, label, "label already removed (404)");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Add a comment to an issue.
    pub async fn add_comment(&self, repo: &str, number: &str, body: &str) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues/{number}/comments");
        let payload = serde_json::json!({ "body": body });
        self.post_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// List comments on an issue.
    pub async fn list_comments(
        &self,
        repo: &str,
        number: &str,
    ) -> anyhow::Result<Vec<GitHubComment>> {
        self.get_json(&format!(
            "{GITHUB_API}/repos/{repo}/issues/{number}/comments"
        ))
        .await
    }

    /// Check if a PR with the given branch was merged.
    pub async fn is_pr_merged(&self, repo: &str, branch: &str) -> anyhow::Result<bool> {
        let owner = repo
            .split('/')
            .next()
            .ok_or_else(|| anyhow::anyhow!("invalid repo format: {}", repo))?;
        let head = format!("{}:{}", owner, branch);

        let url = format!("{GITHUB_API}/repos/{repo}/pulls");
        let prs: Vec<serde_json::Value> = self
            .get_with_query(
                &url,
                &[("head", &head), ("state", "closed"), ("per_page", "1")],
            )
            .await?;

        if prs.is_empty() {
            return Ok(false);
        }
        let merged_at = prs[0].get("merged_at");
        Ok(merged_at.map(|v| !v.is_null()).unwrap_or(false))
    }

    /// Get issue/PR comments since a given timestamp (paginated).
    pub async fn get_mentions(
        &self,
        repo: &str,
        since: &str,
    ) -> anyhow::Result<Vec<GitHubComment>> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues/comments");
        self.get_all_pages(&url, &[("since", since), ("per_page", "100")])
            .await
    }

    /// Get the current authenticated username.
    pub async fn get_whoami(&self) -> anyhow::Result<String> {
        let user: serde_json::Value = self.get_json(&format!("{GITHUB_API}/user")).await?;
        user.get("login")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("failed to get current user"))
    }

    /// Get PR number by branch name.
    pub async fn get_pr_number(&self, repo: &str, branch: &str) -> anyhow::Result<Option<u64>> {
        let owner = repo
            .split('/')
            .next()
            .ok_or_else(|| anyhow::anyhow!("invalid repo format: {}", repo))?;
        let head = format!("{}:{}", owner, branch);

        let url = format!("{GITHUB_API}/repos/{repo}/pulls");
        let prs: Vec<serde_json::Value> = self
            .get_with_query(
                &url,
                &[("head", &head), ("state", "open"), ("per_page", "1")],
            )
            .await?;

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
    pub async fn get_pr_reviews(
        &self,
        repo: &str,
        pr_number: u64,
    ) -> anyhow::Result<Vec<GitHubReview>> {
        self.get_json(&format!(
            "{GITHUB_API}/repos/{repo}/pulls/{pr_number}/reviews"
        ))
        .await
    }

    /// Get review comments for a specific PR review.
    #[allow(dead_code)]
    pub async fn get_pr_review_comments(
        &self,
        repo: &str,
        pr_number: u64,
        review_id: u64,
    ) -> anyhow::Result<Vec<GitHubReviewComment>> {
        self.get_json(&format!(
            "{GITHUB_API}/repos/{repo}/pulls/{pr_number}/reviews/{review_id}/comments"
        ))
        .await
    }

    /// Get all review comments for a PR.
    pub async fn get_pr_comments(
        &self,
        repo: &str,
        pr_number: u64,
    ) -> anyhow::Result<Vec<GitHubReviewComment>> {
        self.get_json(&format!(
            "{GITHUB_API}/repos/{repo}/pulls/{pr_number}/comments"
        ))
        .await
    }

    /// Close a GitHub issue.
    pub async fn close_issue(&self, repo: &str, number: &str) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues/{number}");
        let payload = serde_json::json!({ "state": "closed" });
        self.patch_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Check if a user is a collaborator.
    pub async fn is_collaborator(&self, repo: &str, username: &str) -> anyhow::Result<bool> {
        Self::check_backoff()?;
        let url = format!("{GITHUB_API}/repos/{repo}/collaborators/{username}");
        let resp = self
            .client
            .get(&url)
            .header(header::AUTHORIZATION, self.auth_header())
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;
        Self::record_response(&resp);
        let status = resp.status();
        if status == StatusCode::NO_CONTENT {
            Ok(true)
        } else if status == StatusCode::NOT_FOUND {
            Ok(false)
        } else {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub API collaborator check failed ({status}): {body}");
        }
    }

    /// Add a sub-issue relationship via GraphQL.
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

    /// Merge a PR using the REST API (squash merge).
    pub async fn merge_pr(
        &self,
        repo: &str,
        pr_number: u64,
        delete_branch: bool,
    ) -> anyhow::Result<()> {
        Self::check_backoff()?;
        let url = format!("{GITHUB_API}/repos/{repo}/pulls/{pr_number}/merge");
        let payload = serde_json::json!({ "merge_method": "squash" });
        let resp = self
            .client
            .put(&url)
            .json(&payload)
            .header(header::AUTHORIZATION, self.auth_header())
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;
        Self::record_response(&resp);
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub PR merge failed ({status}): {body}");
        }
        Self::record_success();

        // Delete branch if requested
        if delete_branch {
            // Get the head ref (branch name) from the PR
            let pr: serde_json::Value = self
                .get_json(&format!("{GITHUB_API}/repos/{repo}/pulls/{pr_number}"))
                .await?;
            if let Some(ref_name) = pr.pointer("/head/ref").and_then(|v| v.as_str()) {
                let del_url = format!("{GITHUB_API}/repos/{repo}/git/refs/heads/{ref_name}");
                if let Err(e) = self.delete(&del_url).await {
                    tracing::warn!(ref_name, err = %e, "failed to delete branch after merge");
                }
            }
        }
        Ok(())
    }

    /// Get sub-issues via GraphQL (paginated).
    pub async fn get_sub_issues(&self, repo: &str, number: &str) -> anyhow::Result<Vec<u64>> {
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

            let result = self
                .graphql_with_headers(&query, &["GraphQL-Features:sub_issues"])
                .await?;

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

    /// Get combined CI check status for a git ref.
    pub async fn get_combined_status(
        &self,
        repo: &str,
        git_ref: &str,
    ) -> anyhow::Result<(String, u64, u64, u64, u64)> {
        let url = format!("{GITHUB_API}/repos/{repo}/commits/{git_ref}/check-runs");
        let resp: serde_json::Value = self.get_json(&url).await?;

        let runs = resp
            .get("check_runs")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

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

    /// Re-run failed jobs for a workflow run.
    pub async fn rerun_failed_jobs(&self, repo: &str, run_id: u64) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/actions/runs/{run_id}/rerun-failed-jobs");
        self.post_json_raw(&url, &serde_json::json!({})).await?;
        Ok(())
    }

    /// Get the latest workflow run for a branch.
    pub async fn get_latest_run_for_branch(
        &self,
        repo: &str,
        branch: &str,
    ) -> anyhow::Result<Option<(u64, String, String)>> {
        let url = format!("{GITHUB_API}/repos/{repo}/actions/runs");
        let resp: serde_json::Value = self
            .get_with_query(&url, &[("branch", branch), ("per_page", "1")])
            .await?;

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

    /// Trigger a workflow dispatch.
    pub async fn dispatch_workflow(
        &self,
        repo: &str,
        workflow: &str,
        branch: &str,
    ) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/actions/workflows/{workflow}/dispatches");
        let payload = serde_json::json!({ "ref": branch });
        self.post_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Check the latest automated review comment on a PR.
    pub async fn get_automated_review_status(
        &self,
        repo: &str,
        pr_number: u64,
    ) -> anyhow::Result<Option<String>> {
        let comments = self.list_comments(repo, &pr_number.to_string()).await?;

        let mut latest: Option<&crate::github::types::GitHubComment> = None;
        for c in comments.iter().rev() {
            if !c.body.starts_with("## Automated Review") {
                continue;
            }
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

// ── Token resolution ─────────────────────────────────────────────────

/// Resolve a GitHub token: `GH_TOKEN` env → `GITHUB_TOKEN` env → `gh auth token`.
fn resolve_token() -> String {
    if let Ok(t) = std::env::var("GH_TOKEN") {
        if !t.is_empty() {
            return t;
        }
    }
    if let Ok(t) = std::env::var("GITHUB_TOKEN") {
        if !t.is_empty() {
            return t;
        }
    }
    // Fall back to `gh auth token`
    match std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
    {
        Ok(out) if out.status.success() => {
            let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !token.is_empty() {
                return token;
            }
        }
        _ => {}
    }
    // Try common gh install paths for launchd environments
    for path in &["/opt/homebrew/bin/gh", "/usr/local/bin/gh"] {
        if let Ok(out) = std::process::Command::new(path)
            .args(["auth", "token"])
            .output()
        {
            if out.status.success() {
                let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !token.is_empty() {
                    return token;
                }
            }
        }
    }
    tracing::error!("no GitHub token found: set GH_TOKEN, GITHUB_TOKEN, or run `gh auth login`");
    String::new()
}

// ── Link header parser ───────────────────────────────────────────────

/// Parse the `Link` header to find the `rel="next"` URL.
fn parse_link_next(headers: &header::HeaderMap) -> Option<String> {
    let link = headers.get("link")?.to_str().ok()?;
    for part in link.split(',') {
        let part = part.trim();
        if part.contains("rel=\"next\"") {
            // Extract URL between < and >
            let start = part.find('<')? + 1;
            let end = part.find('>')?;
            return Some(part[start..end].to_string());
        }
    }
    None
}

/// Return the hex color string for a given `status:*` label.
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
    fn parse_link_next_finds_next_url() {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            "link",
            "<https://api.github.com/repos/foo/bar/issues?page=2>; rel=\"next\", <https://api.github.com/repos/foo/bar/issues?page=5>; rel=\"last\""
                .parse()
                .unwrap(),
        );
        assert_eq!(
            parse_link_next(&headers),
            Some("https://api.github.com/repos/foo/bar/issues?page=2".to_string())
        );
    }

    #[test]
    fn parse_link_next_none_when_no_next() {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            "link",
            "<https://api.github.com/repos/foo/bar/issues?page=1>; rel=\"prev\""
                .parse()
                .unwrap(),
        );
        assert_eq!(parse_link_next(&headers), None);
    }

    #[test]
    fn parse_link_next_none_when_missing() {
        let headers = header::HeaderMap::new();
        assert_eq!(parse_link_next(&headers), None);
    }

    #[test]
    fn rate_limit_inactive_by_default() {
        let rl = RateLimit {
            remaining: None,
            reset_at: None,
            backoff_until: None,
            backoff_delay: Duration::ZERO,
            backoff_base: Duration::from_secs(30),
            backoff_max: Duration::from_secs(900),
        };
        assert!(rl.is_active().is_none());
    }

    #[test]
    fn rate_limit_backoff_activates() {
        let mut rl = RateLimit {
            remaining: None,
            reset_at: None,
            backoff_until: None,
            backoff_delay: Duration::ZERO,
            backoff_base: Duration::from_secs(5),
            backoff_max: Duration::from_secs(60),
        };
        rl.record_rate_limit();
        assert!(rl.is_active().is_some());
        assert_eq!(rl.backoff_delay, Duration::from_secs(5));
    }

    #[test]
    fn rate_limit_backoff_doubles() {
        let mut rl = RateLimit {
            remaining: None,
            reset_at: None,
            backoff_until: None,
            backoff_delay: Duration::ZERO,
            backoff_base: Duration::from_secs(5),
            backoff_max: Duration::from_secs(60),
        };
        rl.record_rate_limit();
        assert_eq!(rl.backoff_delay, Duration::from_secs(5));
        rl.record_rate_limit();
        assert_eq!(rl.backoff_delay, Duration::from_secs(10));
        rl.record_rate_limit();
        assert_eq!(rl.backoff_delay, Duration::from_secs(20));
    }

    #[test]
    fn rate_limit_backoff_caps_at_max() {
        let mut rl = RateLimit {
            remaining: None,
            reset_at: None,
            backoff_until: None,
            backoff_delay: Duration::ZERO,
            backoff_base: Duration::from_secs(30),
            backoff_max: Duration::from_secs(60),
        };
        rl.record_rate_limit(); // 30
        rl.record_rate_limit(); // 60
        rl.record_rate_limit(); // capped at 60
        assert_eq!(rl.backoff_delay, Duration::from_secs(60));
    }

    #[test]
    fn rate_limit_success_resets() {
        let mut rl = RateLimit {
            remaining: None,
            reset_at: None,
            backoff_until: None,
            backoff_delay: Duration::ZERO,
            backoff_base: Duration::from_secs(5),
            backoff_max: Duration::from_secs(60),
        };
        rl.record_rate_limit();
        assert!(rl.is_active().is_some());
        rl.record_success();
        assert!(rl.is_active().is_none());
        assert_eq!(rl.backoff_delay, Duration::ZERO);
    }

    #[test]
    fn rate_limit_proactive_pause() {
        let mut rl = RateLimit {
            remaining: None,
            reset_at: None,
            backoff_until: None,
            backoff_delay: Duration::ZERO,
            backoff_base: Duration::from_secs(30),
            backoff_max: Duration::from_secs(900),
        };
        // Simulate: 0 remaining, reset 60s in the future
        rl.remaining = Some(0);
        let future_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 60;
        rl.reset_at = Some(future_epoch);
        let pause = rl.is_active();
        assert!(pause.is_some());
        // Should be roughly 60 seconds (within tolerance)
        assert!(pause.unwrap().as_secs() <= 61);
    }
}
