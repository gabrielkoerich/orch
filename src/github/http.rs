//! Native `reqwest` HTTP client for the GitHub API — replaces `gh` CLI subprocesses.
//!
//! Uses a shared `reqwest::Client` with connection pooling, reads rate-limit
//! headers proactively, and supports concurrent requests via `tokio::join!`.
//!
//! Auth: uses the centralized [`TokenResolver`] which supports:
//! - `GH_TOKEN` / `GITHUB_TOKEN` environment variables
//! - `gh.auth.token` config value
//! - `gh auth token` CLI fallback (default — just run `gh auth login`)
//! - GitHub App JWT generation (with app_id + private_key configuration)

use super::token;
use super::types::{
    GitHubCheckRun, GitHubComment, GitHubIssue, GitHubPullRequest, GitHubReview,
    GitHubReviewComment,
};
use reqwest::{header, Client, Response, StatusCode};
use serde::Serialize;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
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
    /// Hard backoff after a 403/429 (fallback when reset time is absent).
    backoff_until: Option<Instant>,
    backoff_delay: Duration,
    backoff_base: Duration,
    backoff_max: Duration,
    /// Proactively throttle when remaining drops below this threshold (default 10).
    throttle_threshold: u32,
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
        let threshold = crate::config::get("gh.rate_limit.throttle_threshold")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(10);

        Self {
            remaining: None,
            reset_at: None,
            backoff_until: None,
            backoff_delay: Duration::ZERO,
            backoff_base: Duration::from_secs(base),
            backoff_max: Duration::from_secs(max),
            throttle_threshold: threshold,
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

    /// Record a successful call — reset hard backoff.
    fn record_success(&mut self) {
        if self.backoff_delay > Duration::ZERO {
            tracing::info!("GitHub backoff cleared after successful API call");
        }
        self.backoff_delay = Duration::ZERO;
        self.backoff_until = None;
    }

    /// Record a 403/429 — use reset_at timestamp if available, else exponential backoff.
    fn record_rate_limit(&mut self) {
        METRIC_RATE_LIMIT_HITS.fetch_add(1, Ordering::Relaxed);

        // Prefer reset_at when available — avoids over-waiting with exponential backoff
        if let Some(reset_epoch) = self.reset_at {
            let now_epoch = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if reset_epoch > now_epoch {
                let wait_secs = reset_epoch - now_epoch + 1;
                self.backoff_delay = Duration::from_secs(wait_secs);
                self.backoff_until = Some(Instant::now() + self.backoff_delay);
                tracing::warn!(wait_secs, "GitHub rate limit hit, waiting until reset time");
                return;
            }
        }

        // Fall back to exponential backoff when reset time is unknown
        self.backoff_delay = if self.backoff_delay.is_zero() {
            self.backoff_base
        } else {
            (self.backoff_delay * 2).min(self.backoff_max)
        };
        self.backoff_until = Some(Instant::now() + self.backoff_delay);
        tracing::warn!(
            delay_secs = self.backoff_delay.as_secs(),
            "GitHub rate limit hit, backing off (no reset header)"
        );
    }

    /// Returns remaining hard-backoff duration (post-403/429), or None.
    fn backoff_remaining(&self) -> Option<Duration> {
        if let Some(until) = self.backoff_until {
            let now = Instant::now();
            if now < until {
                return Some(until - now);
            }
        }
        None
    }

    /// Returns how long to proactively wait when approaching the rate limit.
    ///
    /// Returns Some only when:
    /// - not already in hard backoff,
    /// - `remaining` is known and below `throttle_threshold`, and
    /// - `reset_at` is known and in the future.
    fn proactive_wait_duration(&self) -> Option<Duration> {
        // Don't double-throttle when already in hard backoff
        if self.backoff_remaining().is_some() {
            return None;
        }
        if let Some(remaining) = self.remaining {
            if remaining < self.throttle_threshold {
                if let Some(reset_epoch) = self.reset_at {
                    let now_epoch = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    if now_epoch < reset_epoch {
                        return Some(Duration::from_secs(reset_epoch - now_epoch + 1));
                    }
                }
            }
        }
        None
    }

    /// Combined check: backoff or proactive throttle. Used by engine's is_rate_limited().
    fn is_active(&self) -> Option<Duration> {
        self.backoff_remaining()
            .or_else(|| self.proactive_wait_duration())
    }

    /// Clear remaining/reset state after a proactive wait (values are now stale).
    fn clear_proactive_state(&mut self) {
        self.remaining = None;
        self.reset_at = None;
    }
}

// ── Global state ─────────────────────────────────────────────────────

static REST_RATE_LIMIT: std::sync::LazyLock<Mutex<RateLimit>> =
    std::sync::LazyLock::new(|| Mutex::new(RateLimit::new()));

/// Separate rate-limit tracker for the GraphQL endpoint (distinct quota from REST).
static GRAPHQL_RATE_LIMIT: std::sync::LazyLock<Mutex<RateLimit>> =
    std::sync::LazyLock::new(|| Mutex::new(RateLimit::new()));

// ── Rate-limit metrics ────────────────────────────────────────────────

/// Total 403/429 rate-limit responses received (REST + GraphQL).
static METRIC_RATE_LIMIT_HITS: AtomicU64 = AtomicU64::new(0);
/// Times proactive throttling slept before sending a request.
static METRIC_PROACTIVE_THROTTLES: AtomicU64 = AtomicU64::new(0);
/// Total seconds spent waiting due to proactive throttle or hard backoff.
static METRIC_WAIT_SECS_TOTAL: AtomicU64 = AtomicU64::new(0);

// ── Shared token resolver ────────────────────────────────────────────

// ── GhHttp client ────────────────────────────────────────────────────

/// Native HTTP client for the GitHub API with connection pooling and
/// proactive rate-limit avoidance.
#[derive(Clone)]
pub struct GhHttp {
    client: Client,
    token_resolver: Arc<token::TokenResolver>,
}

impl GhHttp {
    /// Create a new client backed by the process-wide shared [`TokenResolver`].
    ///
    /// Token is resolved lazily on first request:
    ///   GH_TOKEN → GITHUB_TOKEN → gh.auth.token config → gh auth token CLI
    /// The result is cached in the shared resolver for the life of the process,
    /// so `gh auth token` is only called once regardless of how many instances
    /// are created.
    pub fn new() -> anyhow::Result<Self> {
        let client = Client::builder()
            .user_agent("orch/0.1 (reqwest)")
            .pool_max_idle_per_host(4)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build HTTP client (TLS init): {e}"))?;

        Ok(Self {
            client,
            token_resolver: token::shared(),
        })
    }

    // ── Rate-limit helpers ────────────────────────────────────────

    /// Check if the REST GitHub API is currently rate-limited (backoff or approaching limit).
    /// Used by the engine to decide whether to skip a tick.
    pub fn is_rate_limited() -> Option<Duration> {
        REST_RATE_LIMIT.lock().ok().and_then(|rl| rl.is_active())
    }

    /// Fail fast if the REST API is in hard backoff (post-403/429).
    fn check_backoff() -> anyhow::Result<()> {
        if let Some(remaining) = REST_RATE_LIMIT
            .lock()
            .ok()
            .and_then(|rl| rl.backoff_remaining())
        {
            anyhow::bail!(
                "GitHub API rate-limited, backoff active for {}s",
                remaining.as_secs()
            );
        }
        Ok(())
    }

    /// Fail fast if the GraphQL API is in hard backoff (post-403/429).
    fn check_graphql_backoff() -> anyhow::Result<()> {
        if let Some(remaining) = GRAPHQL_RATE_LIMIT
            .lock()
            .ok()
            .and_then(|rl| rl.backoff_remaining())
        {
            anyhow::bail!(
                "GitHub GraphQL API rate-limited, backoff active for {}s",
                remaining.as_secs()
            );
        }
        Ok(())
    }

    /// Proactively sleep if the REST API quota is running low (remaining < threshold).
    /// Clears stale header state after sleeping so the next request gets fresh values.
    async fn proactive_throttle_rest() {
        let wait = REST_RATE_LIMIT
            .lock()
            .ok()
            .and_then(|rl| rl.proactive_wait_duration());
        if let Some(d) = wait {
            let secs = d.as_secs().max(1);
            METRIC_PROACTIVE_THROTTLES.fetch_add(1, Ordering::Relaxed);
            METRIC_WAIT_SECS_TOTAL.fetch_add(secs, Ordering::Relaxed);
            tracing::warn!(
                wait_secs = secs,
                "approaching GitHub REST rate limit, throttling until reset"
            );
            tokio::time::sleep(d).await;
            if let Ok(mut rl) = REST_RATE_LIMIT.lock() {
                rl.clear_proactive_state();
            }
        }
    }

    /// Proactively sleep if the GraphQL API quota is running low (remaining < threshold).
    async fn proactive_throttle_graphql() {
        let wait = GRAPHQL_RATE_LIMIT
            .lock()
            .ok()
            .and_then(|rl| rl.proactive_wait_duration());
        if let Some(d) = wait {
            let secs = d.as_secs().max(1);
            METRIC_PROACTIVE_THROTTLES.fetch_add(1, Ordering::Relaxed);
            METRIC_WAIT_SECS_TOTAL.fetch_add(secs, Ordering::Relaxed);
            tracing::warn!(
                wait_secs = secs,
                "approaching GitHub GraphQL rate limit, throttling until reset"
            );
            tokio::time::sleep(d).await;
            if let Ok(mut rl) = GRAPHQL_RATE_LIMIT.lock() {
                rl.clear_proactive_state();
            }
        }
    }

    fn record_response(resp: &Response) {
        if let Ok(mut rl) = REST_RATE_LIMIT.lock() {
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

    fn record_graphql_response(resp: &Response) {
        if let Ok(mut rl) = GRAPHQL_RATE_LIMIT.lock() {
            rl.update_from_headers(resp.headers());
            if resp.status() == StatusCode::TOO_MANY_REQUESTS {
                rl.record_rate_limit();
            } else if resp.status().is_success() {
                rl.record_success();
            }
        }
    }

    /// Check response body for rate-limit signals on 403 responses (REST).
    /// Not all 403s are rate limits — some are permission errors.
    fn maybe_record_rate_limit_from_body(status: StatusCode, body: &str) {
        if status == StatusCode::FORBIDDEN {
            let lower = body.to_lowercase();
            if lower.contains("rate limit")
                || lower.contains("abuse detection")
                || lower.contains("secondary rate")
            {
                if let Ok(mut rl) = REST_RATE_LIMIT.lock() {
                    rl.record_rate_limit();
                }
            }
        }
    }

    /// Check response body for rate-limit signals on 403 responses (GraphQL).
    fn maybe_record_graphql_rate_limit_from_body(status: StatusCode, body: &str) {
        if status == StatusCode::FORBIDDEN {
            let lower = body.to_lowercase();
            if lower.contains("rate limit")
                || lower.contains("abuse detection")
                || lower.contains("secondary rate")
            {
                if let Ok(mut rl) = GRAPHQL_RATE_LIMIT.lock() {
                    rl.record_rate_limit();
                }
            }
        }
    }

    fn record_success() {
        if let Ok(mut rl) = REST_RATE_LIMIT.lock() {
            rl.record_success();
        }
    }

    // ── Low-level HTTP helpers ────────────────────────────────────

    async fn auth_header(&self) -> anyhow::Result<String> {
        match self.token_resolver.get_token().await? {
            Some(token) if !token.is_empty() => Ok(format!("Bearer {token}")),
            _ => anyhow::bail!("No GitHub token available — run `gh auth login` or set GH_TOKEN"),
        }
    }

    /// GET request, returns deserialized JSON.
    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> anyhow::Result<T> {
        Self::proactive_throttle_rest().await;
        Self::check_backoff()?;
        let auth = self.auth_header().await?;
        let resp = self
            .client
            .get(url)
            .header(header::AUTHORIZATION, auth)
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
        Self::proactive_throttle_rest().await;
        Self::check_backoff()?;
        let auth = self.auth_header().await?;
        let resp = self
            .client
            .get(url)
            .header(header::AUTHORIZATION, auth)
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
        Self::proactive_throttle_rest().await;
        Self::check_backoff()?;
        let auth = self.auth_header().await?;
        let resp = self
            .client
            .get(url)
            .query(query)
            .header(header::AUTHORIZATION, auth)
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
        Self::proactive_throttle_rest().await;
        Self::check_backoff()?;
        let auth = self.auth_header().await?;
        let resp = self
            .client
            .post(url)
            .json(body)
            .header(header::AUTHORIZATION, auth)
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
        Self::proactive_throttle_rest().await;
        Self::check_backoff()?;
        let auth = self.auth_header().await?;
        let resp = self
            .client
            .patch(url)
            .json(body)
            .header(header::AUTHORIZATION, auth)
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
        Self::proactive_throttle_rest().await;
        Self::check_backoff()?;
        let auth = self.auth_header().await?;
        let resp = self
            .client
            .delete(url)
            .header(header::AUTHORIZATION, auth)
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
        Self::proactive_throttle_rest().await;
        Self::check_backoff()?;
        let mut all: Vec<T> = Vec::new();
        let mut next_url: Option<String> = None;
        let mut is_first = true;
        const MAX_JSON_DECODE_RETRIES: u32 = 2;
        const JSON_DECODE_RETRY_BACKOFF_MS: u64 = 200;

        loop {
            let (request_url, request_query) = if is_first {
                is_first = false;
                (url, Some(query))
            } else {
                let Some(u) = next_url.as_ref() else { break };
                (u.as_str(), None)
            };

            let mut decode_attempt = 0;
            let (page, next) = loop {
                let auth = self.auth_header().await?;
                let mut req = self
                    .client
                    .get(request_url)
                    .header(header::AUTHORIZATION, auth)
                    .header(header::ACCEPT, "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28");
                if let Some(q) = request_query {
                    req = req.query(q);
                }

                let resp = req.send().await?;
                Self::record_response(&resp);
                let status = resp.status();
                let next_from_headers = parse_link_next(resp.headers());
                let content_type = resp
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "<missing>".to_string());

                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    Self::maybe_record_rate_limit_from_body(status, &body);
                    anyhow::bail!("GitHub API GET (paginated) failed ({status}): {body}");
                }

                let text = resp.text().await?;
                match serde_json::from_str::<Vec<T>>(&text) {
                    Ok(page) => break (page, next_from_headers),
                    Err(err) => {
                        let snippet: String = text.chars().take(200).collect();
                        let decode_error = anyhow::anyhow!(
                            "GitHub API GET (paginated) JSON decode failed ({status}, content-type={content_type}): {snippet}"
                        )
                        .context(err);
                        if decode_attempt < MAX_JSON_DECODE_RETRIES {
                            decode_attempt += 1;
                            tracing::warn!(
                                attempt = decode_attempt,
                                max_attempts = MAX_JSON_DECODE_RETRIES,
                                status = %status,
                                content_type,
                                err = %decode_error,
                                "json decode failed; retrying paginated GET"
                            );
                            tokio::time::sleep(Duration::from_millis(
                                JSON_DECODE_RETRY_BACKOFF_MS * u64::from(decode_attempt),
                            ))
                            .await;
                            continue;
                        }
                        return Err(decode_error);
                    }
                }
            };

            next_url = next;
            all.extend(page);

            if next_url.is_none() {
                break;
            }
        }

        Ok(all)
    }

    /// POST to the GraphQL endpoint. Uses separate rate-limit tracking from REST.
    async fn graphql_request(
        &self,
        query: &str,
        extra_headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        Self::proactive_throttle_graphql().await;
        Self::check_graphql_backoff()?;
        let body = serde_json::json!({ "query": query });
        let auth = self.auth_header().await?;
        let mut req = self
            .client
            .post(format!("{GITHUB_API}/graphql"))
            .json(&body)
            .header(header::AUTHORIZATION, auth)
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");

        for (k, v) in extra_headers {
            req = req.header(*k, *v);
        }

        let resp = req.send().await?;
        Self::record_graphql_response(&resp);
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            Self::maybe_record_graphql_rate_limit_from_body(status, &text);
            anyhow::bail!("GitHub GraphQL failed ({status}): {text}");
        }
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
    ///
    /// Returns detailed error messages for common auth failures to help
    /// users diagnose configuration issues at startup.
    pub async fn auth_status(&self) -> anyhow::Result<()> {
        match self
            .get_json::<serde_json::Value>(&format!("{GITHUB_API}/user"))
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                let err_str = e.to_string();

                // Provide helpful error messages for common auth failures
                if err_str.contains("401") || err_str.contains("Bad credentials") {
                    anyhow::bail!(
                        "GitHub authentication failed: Invalid or expired token. \
                         Check your GH_TOKEN/GITHUB_TOKEN environment variables, \
                         or gh.auth configuration in ~/.orch/config.yml. \
                         Run `orch auth check` to verify your setup."
                    );
                }

                if err_str.contains("403") && err_str.contains("rate limit") {
                    anyhow::bail!(
                        "GitHub API rate limited. Wait before retrying, \
                         or check your token's rate limit quota."
                    );
                }

                Err(e)
            }
        }
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
        self.list_issues_with_state(repo, label, "open").await
    }

    /// List issues filtered by a label and issue state (paginated).
    ///
    /// `state` is passed directly to the GitHub API: `"open"`, `"closed"`, or `"all"`.
    pub async fn list_issues_with_state(
        &self,
        repo: &str,
        label: &str,
        state: &str,
    ) -> anyhow::Result<Vec<GitHubIssue>> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues");
        let all: Vec<GitHubIssue> = self
            .get_all_pages(
                &url,
                &[("labels", label), ("state", state), ("per_page", "100")],
            )
            .await?;
        // GitHub /issues API returns PRs too — filter them out
        Ok(all
            .into_iter()
            .filter(|i| i.pull_request.is_none())
            .collect())
    }

    /// List closed issues updated since `since` (ISO 8601), filtered by label.
    ///
    /// Used for dedup checks — prevents re-creating issues that were recently closed.
    pub async fn list_issues_closed_since(
        &self,
        repo: &str,
        label: &str,
        since: &str,
    ) -> anyhow::Result<Vec<GitHubIssue>> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues");
        let all: Vec<GitHubIssue> = self
            .get_all_pages(
                &url,
                &[
                    ("labels", label),
                    ("state", "closed"),
                    ("since", since),
                    ("per_page", "100"),
                ],
            )
            .await?;
        Ok(all
            .into_iter()
            .filter(|i| i.pull_request.is_none())
            .collect())
    }

    /// List closed issues updated since `since` (ISO 8601), no label filter.
    pub async fn list_closed_issues_since(
        &self,
        repo: &str,
        since: &str,
    ) -> anyhow::Result<Vec<GitHubIssue>> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues");
        let all: Vec<GitHubIssue> = self
            .get_all_pages(
                &url,
                &[("state", "closed"), ("since", since), ("per_page", "100")],
            )
            .await?;
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

    /// List all issues (open and closed, no label filter, paginated).
    ///
    /// This is more efficient than making separate calls for open and closed.
    pub async fn list_all_issues(&self, repo: &str) -> anyhow::Result<Vec<GitHubIssue>> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues");
        let all: Vec<GitHubIssue> = self
            .get_all_pages(&url, &[("state", "all"), ("per_page", "100")])
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
        let url = format!("{GITHUB_API}/repos/{repo}/issues/{number}/comments");
        self.get_all_pages(&url, &[("per_page", "100")]).await
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

    /// Batch-check whether the PR for each branch was merged via a single GraphQL query.
    ///
    /// Returns a map of branch → merged. Branches with no matching PR are omitted
    /// (callers should treat missing entries as not merged).
    pub async fn batch_is_pr_merged_by_branch(
        &self,
        repo: &str,
        branches: &[String],
    ) -> anyhow::Result<std::collections::HashMap<String, bool>> {
        if branches.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("invalid repo format: {}", repo))?;

        // Build one alias per branch using positional index to avoid GraphQL
        // alias restrictions on branch names that contain non-identifier chars.
        let aliases: String = branches
            .iter()
            .enumerate()
            .map(|(i, branch)| {
                let escaped = branch.replace('\\', "\\\\").replace('"', "\\\"");
                format!(
                    r#"b{i}: pullRequests(headRefName: \"{escaped}\", states: [MERGED], last: 1) {{ nodes {{ merged }} }}"#
                )
            })
            .collect::<Vec<_>>()
            .join(" ");

        let query = format!(
            r#"{{ "query": "{{ repository(owner: \"{owner}\", name: \"{name}\") {{ {aliases} }} }}" }}"#
        );

        let resp = self.graphql(&query).await?;
        let repo_data = resp
            .pointer("/data/repository")
            .ok_or_else(|| anyhow::anyhow!("missing /data/repository in GraphQL response"))?;

        let mut result = std::collections::HashMap::new();
        for (i, branch) in branches.iter().enumerate() {
            let alias = format!("b{i}");
            let merged = repo_data
                .get(&alias)
                .and_then(|v| v.get("nodes"))
                .and_then(|nodes| nodes.as_array())
                .and_then(|arr| arr.first())
                .and_then(|node| node.get("merged"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            result.insert(branch.clone(), merged);
        }

        Ok(result)
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

    /// Get all comments for a specific issue number.
    pub async fn get_issue_comments(
        &self,
        repo: &str,
        issue_number: u64,
    ) -> anyhow::Result<Vec<GitHubComment>> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues/{issue_number}/comments");
        self.get_all_pages(&url, &[("per_page", "100")]).await
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

    /// Create a new pull request.
    ///
    /// # Arguments
    /// * `repo` - Repository in "owner/repo" format
    /// * `title` - PR title
    /// * `body` - PR body/description
    /// * `head` - Branch name to create PR from
    /// * `base` - Base branch to merge into (e.g., "main")
    ///
    /// Returns the created PR's URL on success.
    pub async fn create_pr(
        &self,
        repo: &str,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
    ) -> anyhow::Result<String> {
        use reqwest::header;

        Self::proactive_throttle_rest().await;
        Self::check_backoff()?;
        let url = format!("{GITHUB_API}/repos/{repo}/pulls");

        #[derive(Serialize)]
        struct CreatePullRequest<'a> {
            title: &'a str,
            body: &'a str,
            head: &'a str,
            base: &'a str,
        }

        let payload = CreatePullRequest {
            title,
            body,
            head,
            base,
        };

        let auth = self.auth_header().await?;
        let response = self
            .client
            .post(&url)
            .header(header::AUTHORIZATION, auth)
            .header(header::ACCEPT, "application/vnd.github+json")
            .header(header::USER_AGENT, "orch")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&payload)
            .send()
            .await?;

        Self::record_response(&response);
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            Self::maybe_record_rate_limit_from_body(status, &text);
            anyhow::bail!("GitHub API POST {url} failed ({status}): {text}");
        }

        let pr: serde_json::Value = serde_json::from_str(&text)?;
        pr.get("html_url")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("failed to get PR URL from response: {:?}", pr))
    }

    /// Get PR details by PR number.
    /// Returns the full PR object including the `mergeable` field.
    pub async fn get_pr(&self, repo: &str, pr_number: u64) -> anyhow::Result<GitHubPullRequest> {
        let url = format!("{GITHUB_API}/repos/{repo}/pulls/{pr_number}");
        self.get_json(&url).await
    }

    /// Fetch the merged/state for multiple PRs in a single GraphQL query.
    ///
    /// Returns a map of pr_number → (merged, state).
    /// PRs that fail to fetch are silently omitted from the result.
    pub async fn batch_get_pr_states(
        &self,
        repo: &str,
        pr_numbers: &[u64],
    ) -> anyhow::Result<std::collections::HashMap<u64, (bool, String)>> {
        if pr_numbers.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("invalid repo format: {}", repo))?;

        // Build GraphQL aliases: pr123: pullRequest(number: 123) { merged state }
        let aliases: String = pr_numbers
            .iter()
            .map(|n| format!("pr{n}: pullRequest(number: {n}) {{ merged state }}"))
            .collect::<Vec<_>>()
            .join("\n  ");

        let query = format!(
            r#"{{ "query": "{{ repository(owner: \"{owner}\", name: \"{name}\") {{ {aliases} }} }}" }}"#
        );

        let resp = self.graphql(&query).await?;
        let repo_data = resp
            .pointer("/data/repository")
            .ok_or_else(|| anyhow::anyhow!("missing /data/repository in GraphQL response"))?;

        let mut result = std::collections::HashMap::new();
        for n in pr_numbers {
            if let Some(pr) = repo_data.get(format!("pr{n}")) {
                let merged = pr.get("merged").and_then(|v| v.as_bool()).unwrap_or(false);
                let state = pr
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap_or("UNKNOWN")
                    .to_lowercase();
                result.insert(*n, (merged, state));
            }
        }
        Ok(result)
    }

    /// Get the required status check contexts for a branch.
    pub async fn get_required_status_check_contexts(
        &self,
        repo: &str,
        branch: &str,
    ) -> anyhow::Result<Vec<String>> {
        let branch = urlencoding::encode(branch);
        let url = format!("{GITHUB_API}/repos/{repo}/branches/{branch}/protection");
        let resp = self.get_json::<serde_json::Value>(&url).await;
        match resp {
            Ok(value) => Ok(value
                .pointer("/required_status_checks/contexts")
                .and_then(|v| v.as_array())
                .map(|contexts| {
                    contexts
                        .iter()
                        .filter_map(|context| context.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default()),
            Err(e) => {
                let err = e.to_string();
                if err.contains("404") {
                    Ok(vec![])
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Get commit status contexts for a SHA.
    pub async fn get_commit_status_contexts(
        &self,
        repo: &str,
        sha: &str,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let url = format!("{GITHUB_API}/repos/{repo}/commits/{sha}/status");
        let resp = self.get_json::<serde_json::Value>(&url).await?;
        Ok(resp
            .get("statuses")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|status| {
                let context = status.get("context").and_then(|v| v.as_str())?;
                let state = status.get("state").and_then(|v| v.as_str())?;
                Some((context.to_string(), state.to_string()))
            })
            .collect())
    }

    fn combined_status_state(
        total: u64,
        failing: u64,
        pending: u64,
        has_workflows: bool,
    ) -> String {
        if failing > 0 {
            "failure".to_string()
        } else if pending > 0 || (total == 0 && has_workflows) {
            "pending".to_string()
        } else {
            "success".to_string()
        }
    }

    /// Return `true` when the repository has at least one GitHub Actions workflow.
    ///
    /// This is used to distinguish repos that genuinely have no CI configured from
    /// repos where CI exists but GitHub has not surfaced any runs yet.
    pub async fn has_workflows(&self, repo: &str) -> anyhow::Result<bool> {
        let url = format!("{GITHUB_API}/repos/{repo}/actions/workflows?per_page=1");
        let resp = self.get_json::<serde_json::Value>(&url).await;
        match resp {
            Ok(value) => Ok(value
                .get("workflows")
                .and_then(|v| v.as_array())
                .map(|workflows| !workflows.is_empty())
                .unwrap_or(false)),
            Err(e) => {
                let err = e.to_string();
                if err.contains("404") {
                    Ok(false)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Get check runs for a PR head SHA.
    pub async fn get_check_runs(
        &self,
        repo: &str,
        sha: &str,
    ) -> anyhow::Result<Vec<GitHubCheckRun>> {
        // Use `filter=latest` so GitHub returns only the most recent check run
        // per check name. Without this, the API returns all runs (oldest
        // first) and callers that pick the first match may observe a stale
        // failed run when a newer run with the same name has succeeded.
        let url = format!("{GITHUB_API}/repos/{repo}/commits/{sha}/check-runs?filter=latest");
        let resp: serde_json::Value = self.get_json(&url).await?;
        Ok(resp
            .get("check_runs")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|run| serde_json::from_value(run).ok())
            .collect())
    }

    /// Get reviews for a PR.
    pub async fn get_pr_reviews(
        &self,
        repo: &str,
        pr_number: u64,
    ) -> anyhow::Result<Vec<GitHubReview>> {
        let url = format!("{GITHUB_API}/repos/{repo}/pulls/{pr_number}/reviews");
        self.get_all_pages(&url, &[("per_page", "100")]).await
    }

    /// Get the committer timestamp for a specific commit SHA.
    ///
    /// Returns an ISO-8601 string, e.g. `"2024-01-15T12:00:00Z"`.
    /// Used to determine whether the branch has been updated since a review was submitted.
    pub async fn get_commit_timestamp(&self, repo: &str, sha: &str) -> anyhow::Result<String> {
        let url = format!("{GITHUB_API}/repos/{repo}/commits/{sha}");
        let value: serde_json::Value = self.get_json(&url).await?;
        value
            .pointer("/commit/committer/date")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("commit {sha} has no committer date"))
    }

    /// Update the body of a pull request.
    pub async fn update_pr_body(
        &self,
        repo: &str,
        pr_number: u64,
        body: &str,
    ) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/pulls/{pr_number}");
        let payload = serde_json::json!({ "body": body });
        self.patch_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Close a GitHub issue.
    pub async fn close_issue(&self, repo: &str, number: &str) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues/{number}");
        let payload = serde_json::json!({ "state": "closed" });
        self.patch_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Reopen a closed GitHub issue.
    pub async fn reopen_issue(&self, repo: &str, number: &str) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues/{number}");
        let payload = serde_json::json!({ "state": "open" });
        self.patch_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Check if a user is a collaborator.
    pub async fn is_collaborator(&self, repo: &str, username: &str) -> anyhow::Result<bool> {
        Self::proactive_throttle_rest().await;
        Self::check_backoff()?;
        let url = format!("{GITHUB_API}/repos/{repo}/collaborators/{username}");
        let auth = self.auth_header().await?;
        let resp = self
            .client
            .get(&url)
            .header(header::AUTHORIZATION, auth)
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

    /// Link an issue to a branch using GraphQL.
    ///
    /// Creates a "Development" sidebar link in GitHub that connects the issue
    /// to the branch, similar to `gh issue develop`.
    ///
    /// # Arguments
    /// * `repo` - Repository in "owner/repo" format
    /// * `issue_number` - The issue number to link
    /// * `branch` - The branch name to link the issue to
    ///
    /// Returns the linked branch ID on success.
    pub async fn link_issue_to_branch(
        &self,
        repo: &str,
        issue_number: u64,
        branch: &str,
    ) -> anyhow::Result<String> {
        let parts: Vec<&str> = repo.split('/').collect();
        if parts.len() != 2 {
            anyhow::bail!("invalid repo format: expected 'owner/repo', got '{}'", repo);
        }
        let (owner, repo_name) = (parts[0], parts[1]);

        // First, get the repository and issue node IDs
        let query = format!(
            r#"{{
                repository(owner: "{}", name: "{}") {{
                    id
                    issue(number: {}) {{
                        id
                    }}
                }}
            }}"#,
            owner, repo_name, issue_number
        );

        let result = self.graphql(&query).await?;

        let repo_id = result
            .pointer("/data/repository/id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("failed to get repository node ID"))?;

        let issue_id = result
            .pointer("/data/repository/issue/id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("failed to get issue node ID"))?;

        // Fetch branch ref to get the commit OID required by createLinkedBranch
        let branch_query = format!(
            r#"{{
                repository(owner: "{}", name: "{}") {{
                    ref(qualifiedName: "refs/heads/{}") {{
                        target {{
                            oid
                        }}
                    }}
                }}
            }}"#,
            owner, repo_name, branch
        );

        let branch_result = self.graphql(&branch_query).await?;
        let branch_oid = branch_result
            .pointer("/data/repository/ref/target/oid")
            .and_then(|v| v.as_str());

        // If branch doesn't exist yet, use the default branch OID.
        // createLinkedBranch will create the branch at that OID AND link it.
        let branch_oid = match branch_oid {
            Some(oid) => oid.to_string(),
            None => {
                // Get default branch OID as the base for the new branch
                let default_query = format!(
                    r#"{{
                        repository(owner: "{}", name: "{}") {{
                            defaultBranchRef {{
                                target {{
                                    oid
                                }}
                            }}
                        }}
                    }}"#,
                    owner, repo_name
                );
                let default_result = self.graphql(&default_query).await?;
                match default_result
                    .pointer("/data/repository/defaultBranchRef/target/oid")
                    .and_then(|v| v.as_str())
                {
                    Some(oid) => {
                        tracing::debug!(
                            repo,
                            issue_number,
                            branch,
                            "branch not found, using default branch OID to create and link"
                        );
                        oid.to_string()
                    }
                    None => {
                        return Ok(format!("unlinked:{}", branch));
                    }
                }
            }
        };

        // Use createLinkedBranch mutation to link the issue to the existing branch.
        // Required fields: issueId, repositoryId, name (branch name), oid (commit SHA).
        let mutation = format!(
            r#"mutation {{
                createLinkedBranch(input: {{
                    issueId: "{issue_id}"
                    repositoryId: "{repo_id}"
                    name: "{branch}"
                    oid: "{branch_oid}"
                }}) {{
                    linkedBranch {{
                        id
                        ref {{
                            name
                        }}
                    }}
                }}
            }}"#,
            issue_id = issue_id,
            repo_id = repo_id,
            branch = branch,
            branch_oid = branch_oid,
        );

        let link_result = self.graphql(&mutation).await;

        match link_result {
            Ok(result) => {
                let linked_branch_id = result
                    .pointer("/data/createLinkedBranch/linkedBranch/id")
                    .and_then(|v| v.as_str())
                    .map(String::from);

                if let Some(id) = linked_branch_id {
                    tracing::info!(
                        repo,
                        issue_number,
                        branch,
                        "linked issue to branch via GraphQL API"
                    );
                    Ok(id)
                } else {
                    // Branch may already be linked - check for errors
                    let errors = result
                        .get("errors")
                        .and_then(|e| e.as_array())
                        .cloned()
                        .unwrap_or_default();

                    if errors.iter().any(|e| {
                        e.get("message")
                            .and_then(|m| m.as_str())
                            .map(|m| m.contains("already"))
                            .unwrap_or(false)
                    }) {
                        tracing::debug!(
                            repo,
                            issue_number,
                            branch,
                            "issue already linked to branch"
                        );
                        Ok(format!("already_linked:{}", branch))
                    } else {
                        anyhow::bail!("failed to link issue to branch: {:?}", result)
                    }
                }
            }
            Err(e) => {
                // Check if it's a "already exists" type error
                let err_str = format!("{}", e);
                if err_str.contains("already") || err_str.contains("existing") {
                    tracing::debug!(
                        repo,
                        issue_number,
                        branch,
                        "issue already linked to branch (error check)"
                    );
                    Ok(format!("already_linked:{}", branch))
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Merge a PR using the REST API (squash merge).
    pub async fn merge_pr(
        &self,
        repo: &str,
        pr_number: u64,
        delete_branch: bool,
    ) -> anyhow::Result<()> {
        Self::proactive_throttle_rest().await;
        Self::check_backoff()?;
        let url = format!("{GITHUB_API}/repos/{repo}/pulls/{pr_number}/merge");
        let payload = serde_json::json!({ "merge_method": "squash" });
        let auth = self.auth_header().await?;
        let resp = self
            .client
            .put(&url)
            .json(&payload)
            .header(header::AUTHORIZATION, auth)
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

    /// Enable auto-merge on a PR via GraphQL.
    ///
    /// GitHub will automatically merge the PR once all required checks pass.
    pub async fn enable_auto_merge(&self, repo: &str, pr_number: u64) -> anyhow::Result<()> {
        let parts: Vec<&str> = repo.split('/').collect();
        if parts.len() != 2 {
            anyhow::bail!("invalid repo format: {repo}");
        }
        let (owner, repo_name) = (parts[0], parts[1]);

        // First get the PR node ID
        let query = format!(
            r#"{{"query":"query {{ repository(owner:\"{owner}\", name:\"{repo_name}\") {{ pullRequest(number:{pr_number}) {{ id }} }} }}"}}"#
        );
        let auth = self.auth_header().await?;
        let resp = self
            .client
            .post(format!("{GITHUB_API}/graphql"))
            .body(query)
            .header(header::AUTHORIZATION, &auth)
            .header(header::ACCEPT, "application/vnd.github+json")
            .send()
            .await?;
        Self::record_response(&resp);
        let body: serde_json::Value = resp.json().await?;
        let pr_id = body
            .pointer("/data/repository/pullRequest/id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("could not get PR node ID"))?
            .to_string();

        // Enable auto-merge with squash
        let mutation = format!(
            r#"{{"query":"mutation {{ enablePullRequestAutoMerge(input: {{pullRequestId: \"{pr_id}\", mergeMethod: SQUASH}}) {{ pullRequest {{ autoMergeRequest {{ enabledAt }} }} }} }}"}}"#
        );
        let resp = self
            .client
            .post(format!("{GITHUB_API}/graphql"))
            .body(mutation)
            .header(header::AUTHORIZATION, &auth)
            .header(header::ACCEPT, "application/vnd.github+json")
            .send()
            .await?;
        Self::record_response(&resp);
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("enable auto-merge failed ({status}): {body}");
        }
        let body: serde_json::Value = resp.json().await?;
        if let Some(errors) = body.get("errors") {
            anyhow::bail!("enable auto-merge GraphQL error: {errors}");
        }

        tracing::info!(repo, pr_number, "auto-merge enabled on PR");
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
        has_workflows: bool,
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
                "completed" => {
                    if conclusion.is_empty() {
                        pending += 1;
                    } else if matches!(conclusion, "success" | "neutral" | "skipped") {
                        passing += 1;
                    } else {
                        failing += 1;
                    }
                }
                _ => pending += 1,
            }
        }

        let total = runs.len() as u64;
        let state = Self::combined_status_state(total, failing, pending, has_workflows);

        Ok((state, total, passing, failing, pending))
    }

    /// Rerun the latest failed workflow run for a given workflow name and branch.
    ///
    /// Finds the most recent failed (or cancelled) run of `workflow_name` on
    /// `branch` triggered by a `push` event, then calls the GitHub Actions
    /// rerun endpoint so the **same check run** on the commit flips green.
    pub async fn rerun_failed_workflow(
        &self,
        repo: &str,
        workflow_name: &str,
        branch: &str,
    ) -> anyhow::Result<()> {
        // List recent runs for this branch, filtered to push events
        let url = format!(
            "{GITHUB_API}/repos/{repo}/actions/runs?branch={branch}&event=push&per_page=10"
        );
        let resp: serde_json::Value = self.get_json(&url).await?;
        let runs = resp["workflow_runs"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("no workflow_runs array in response"))?;

        // Find the latest failed/cancelled run matching the workflow name
        let run_id = runs
            .iter()
            .find(|r| {
                r["name"].as_str() == Some(workflow_name)
                    && matches!(
                        r["conclusion"].as_str(),
                        Some("failure") | Some("cancelled")
                    )
            })
            .and_then(|r| r["id"].as_u64())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no failed run found for workflow '{}' on branch '{}'",
                    workflow_name,
                    branch
                )
            })?;

        // POST rerun — no body needed
        let rerun_url = format!("{GITHUB_API}/repos/{repo}/actions/runs/{run_id}/rerun");
        let auth = self.auth_header().await?;
        let resp = self
            .client
            .post(&rerun_url)
            .header(header::AUTHORIZATION, auth)
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;
        Self::record_response(&resp);
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            Self::maybe_record_rate_limit_from_body(status, &body);
            anyhow::bail!("GitHub API POST rerun {rerun_url} failed ({status}): {body}");
        }

        Ok(())
    }

    /// Fetch reviews, review comments, and issue comments for multiple PRs in a
    /// single GraphQL query instead of 4–5 REST calls per PR.
    ///
    /// Returns a map of `pr_number → PrReviewBatchData`.
    /// PRs missing from the response are omitted from the result.
    pub async fn batch_fetch_pr_review_data(
        &self,
        repo: &str,
        pr_numbers: &[u64],
    ) -> anyhow::Result<std::collections::HashMap<u64, PrReviewBatchData>> {
        if pr_numbers.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("invalid repo format: {}", repo))?;

        // Build one GraphQL alias per PR number.
        let aliases: String = pr_numbers
            .iter()
            .map(|&n| build_pr_review_alias(n))
            .collect::<Vec<_>>()
            .join("\n");

        let query =
            format!(r#"{{ repository(owner: "{owner}", name: "{name}") {{ {aliases} }} }}"#);

        let resp = self.graphql(&query).await?;
        let repo_data = resp
            .pointer("/data/repository")
            .ok_or_else(|| anyhow::anyhow!("missing /data/repository in GraphQL response"))?;

        let mut result = std::collections::HashMap::new();
        for &n in pr_numbers {
            let Some(pr_data) = repo_data.get(format!("pr{n}")) else {
                continue;
            };

            let merged = pr_data
                .get("merged")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mergeable = match pr_data.get("mergeable").and_then(|v| v.as_str()) {
                Some("MERGEABLE") => Some(true),
                Some("CONFLICTING") => Some(false),
                _ => None, // UNKNOWN or missing
            };

            let reviews: Vec<GitHubReview> = pr_data
                .pointer("/reviews/nodes")
                .and_then(|v| v.as_array())
                .map(|nodes| nodes.iter().filter_map(parse_graphql_review).collect())
                .unwrap_or_default();

            let review_comments: Vec<GitHubReviewComment> = pr_data
                .pointer("/reviewThreads/nodes")
                .and_then(|v| v.as_array())
                .map(|threads| {
                    threads
                        .iter()
                        .filter_map(|thread| {
                            thread
                                .pointer("/comments/nodes")
                                .and_then(|v| v.as_array())
                                .map(|nodes| {
                                    nodes
                                        .iter()
                                        .filter_map(parse_graphql_review_comment)
                                        .collect::<Vec<_>>()
                                })
                        })
                        .flatten()
                        .collect()
                })
                .unwrap_or_default();

            let issue_comments: Vec<GitHubComment> = pr_data
                .pointer("/comments/nodes")
                .and_then(|v| v.as_array())
                .map(|nodes| {
                    nodes
                        .iter()
                        .filter_map(parse_graphql_issue_comment)
                        .collect()
                })
                .unwrap_or_default();

            result.insert(
                n,
                PrReviewBatchData {
                    merged,
                    mergeable,
                    reviews,
                    review_comments,
                    issue_comments,
                },
            );
        }

        Ok(result)
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

// ── Public metrics ───────────────────────────────────────────────────

/// Snapshot of GitHub API rate-limit metrics.
#[derive(Debug, Clone)]
pub struct RateLimitMetrics {
    /// Total 403/429 rate-limit responses received (REST + GraphQL combined).
    pub hits: u64,
    /// Times proactive throttling slept before sending a request.
    pub proactive_throttles: u64,
    /// Total seconds spent waiting due to proactive throttle or hard backoff.
    pub wait_secs: u64,
    /// Current REST remaining quota (None if not yet observed).
    pub rest_remaining: Option<u32>,
    /// Current GraphQL remaining quota (None if not yet observed).
    pub graphql_remaining: Option<u32>,
}

/// Return a snapshot of GitHub API rate-limit metrics.
pub fn rate_limit_metrics() -> RateLimitMetrics {
    let rest_remaining = REST_RATE_LIMIT.lock().ok().and_then(|rl| rl.remaining);
    let graphql_remaining = GRAPHQL_RATE_LIMIT.lock().ok().and_then(|rl| rl.remaining);
    RateLimitMetrics {
        hits: METRIC_RATE_LIMIT_HITS.load(Ordering::Relaxed),
        proactive_throttles: METRIC_PROACTIVE_THROTTLES.load(Ordering::Relaxed),
        wait_secs: METRIC_WAIT_SECS_TOTAL.load(Ordering::Relaxed),
        rest_remaining,
        graphql_remaining,
    }
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

// ── Batch GraphQL helpers ─────────────────────────────────────────────

/// Data fetched for one PR by [`GhHttp::batch_fetch_pr_review_data`].
#[derive(Debug, Clone, Default)]
pub struct PrReviewBatchData {
    /// True when the pull request was merged.
    pub merged: bool,
    /// `Some(true)` = MERGEABLE, `Some(false)` = CONFLICTING, `None` = UNKNOWN.
    pub mergeable: Option<bool>,
    /// All reviews submitted on the PR.
    pub reviews: Vec<GitHubReview>,
    /// All inline review comments (from all review threads).
    pub review_comments: Vec<GitHubReviewComment>,
    /// All issue-level comments on the PR.
    pub issue_comments: Vec<GitHubComment>,
}

/// Build a GraphQL alias fragment for one pull request, fetching all data
/// needed by the review poll loop in a single batch request.
fn build_pr_review_alias(n: u64) -> String {
    format!(
        r#"pr{n}: pullRequest(number: {n}) {{
  merged
  mergeable
  reviews(first: 50) {{
    nodes {{
      databaseId
      author {{ login }}
      body
      state
      url
      submittedAt
      commit {{ oid }}
    }}
  }}
  reviewThreads(first: 50) {{
    nodes {{
      comments(first: 20) {{
        nodes {{
          databaseId
          author {{ login }}
          body
          path
          line
          originalLine
          commit {{ oid }}
          originalCommit {{ oid }}
          url
          createdAt
          updatedAt
          replyTo {{ databaseId }}
          diffHunk
        }}
      }}
    }}
  }}
  comments(first: 50) {{
    nodes {{
      databaseId
      author {{ login }}
      body
      createdAt
      url
    }}
  }}
}}"#
    )
}

fn parse_graphql_review(node: &serde_json::Value) -> Option<GitHubReview> {
    use super::types::GitHubUser;
    Some(GitHubReview {
        id: node.get("databaseId").and_then(|v| v.as_u64())?,
        user: GitHubUser {
            login: node
                .pointer("/author/login")
                .and_then(|v| v.as_str())?
                .to_string(),
        },
        body: node.get("body").and_then(|v| v.as_str()).map(String::from),
        state: node.get("state").and_then(|v| v.as_str())?.to_string(),
        html_url: node.get("url").and_then(|v| v.as_str()).map(String::from),
        submitted_at: node
            .get("submittedAt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        commit_id: node
            .pointer("/commit/oid")
            .and_then(|v| v.as_str())
            .map(String::from),
    })
}

fn parse_graphql_review_comment(node: &serde_json::Value) -> Option<GitHubReviewComment> {
    use super::types::GitHubUser;
    Some(GitHubReviewComment {
        id: node.get("databaseId").and_then(|v| v.as_u64())?,
        user: GitHubUser {
            login: node
                .pointer("/author/login")
                .and_then(|v| v.as_str())?
                .to_string(),
        },
        body: node.get("body").and_then(|v| v.as_str())?.to_string(),
        path: node
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        line: node.get("line").and_then(|v| v.as_u64()).map(|v| v as u32),
        original_line: node
            .get("originalLine")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        commit_id: node
            .pointer("/commit/oid")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        original_commit_id: node
            .pointer("/originalCommit/oid")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        html_url: node
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        created_at: node
            .get("createdAt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        updated_at: node
            .get("updatedAt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        in_reply_to_id: node.pointer("/replyTo/databaseId").and_then(|v| v.as_u64()),
        diff_hunk: node
            .get("diffHunk")
            .and_then(|v| v.as_str())
            .map(String::from),
    })
}

fn parse_graphql_issue_comment(node: &serde_json::Value) -> Option<GitHubComment> {
    use super::types::GitHubUser;
    Some(GitHubComment {
        id: node.get("databaseId").and_then(|v| v.as_u64())?,
        body: node.get("body").and_then(|v| v.as_str())?.to_string(),
        user: GitHubUser {
            login: node
                .pointer("/author/login")
                .and_then(|v| v.as_str())?
                .to_string(),
        },
        created_at: node
            .get("createdAt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        updated_at: node
            .get("updatedAt")
            .and_then(|v| v.as_str())
            .map(String::from),
        html_url: node.get("url").and_then(|v| v.as_str()).map(String::from),
        issue_url: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify `GhHttp::new()` returns `Ok` and does not panic on construction.
    ///
    /// This test guards against regressions where `GhHttp::new()` might panic
    /// (e.g., if TLS initialisation fails or the builder API changes).
    #[test]
    fn gh_http_new_returns_ok() {
        let result = GhHttp::new();
        assert!(
            result.is_ok(),
            "GhHttp::new() must not panic and must return Ok; got: {:?}",
            result.err()
        );
    }

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
    fn parse_link_next_none_when_malformed() {
        let mut headers = header::HeaderMap::new();
        // Malformed: no angle-bracket URL, just garbage
        headers.insert("link", "garbage; rel=\"next\"".parse().unwrap());
        assert_eq!(parse_link_next(&headers), None);
    }

    fn make_rl(base_secs: u64, max_secs: u64) -> RateLimit {
        RateLimit {
            remaining: None,
            reset_at: None,
            backoff_until: None,
            backoff_delay: Duration::ZERO,
            backoff_base: Duration::from_secs(base_secs),
            backoff_max: Duration::from_secs(max_secs),
            throttle_threshold: 10,
        }
    }

    #[test]
    fn rate_limit_inactive_by_default() {
        let rl = make_rl(30, 900);
        assert!(rl.is_active().is_none());
    }

    #[test]
    fn rate_limit_backoff_activates() {
        let mut rl = make_rl(5, 60);
        rl.record_rate_limit();
        assert!(rl.is_active().is_some());
        assert_eq!(rl.backoff_delay, Duration::from_secs(5));
    }

    #[test]
    fn rate_limit_backoff_doubles() {
        let mut rl = make_rl(5, 60);
        rl.record_rate_limit();
        assert_eq!(rl.backoff_delay, Duration::from_secs(5));
        rl.record_rate_limit();
        assert_eq!(rl.backoff_delay, Duration::from_secs(10));
        rl.record_rate_limit();
        assert_eq!(rl.backoff_delay, Duration::from_secs(20));
    }

    #[test]
    fn rate_limit_backoff_caps_at_max() {
        let mut rl = make_rl(30, 60);
        rl.record_rate_limit(); // 30
        rl.record_rate_limit(); // 60
        rl.record_rate_limit(); // capped at 60
        assert_eq!(rl.backoff_delay, Duration::from_secs(60));
    }

    #[test]
    fn rate_limit_success_resets() {
        let mut rl = make_rl(5, 60);
        rl.record_rate_limit();
        assert!(rl.is_active().is_some());
        rl.record_success();
        assert!(rl.is_active().is_none());
        assert_eq!(rl.backoff_delay, Duration::ZERO);
    }

    #[test]
    fn rate_limit_proactive_pause() {
        let mut rl = make_rl(30, 900);
        // Simulate: 0 remaining (below default threshold of 10), reset 60s in the future
        rl.remaining = Some(0);
        let future_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 60;
        rl.reset_at = Some(future_epoch);
        let pause = rl.is_active();
        assert!(pause.is_some());
        // Should be roughly 60 seconds (within tolerance, +1s buffer added)
        assert!(pause.unwrap().as_secs() <= 62);
    }

    #[test]
    fn rate_limit_proactive_pause_threshold() {
        let mut rl = make_rl(30, 900);
        // remaining=15 is above default threshold of 10 — no proactive wait
        rl.remaining = Some(15);
        let future_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 60;
        rl.reset_at = Some(future_epoch);
        assert!(rl.is_active().is_none());

        // remaining=5 is below threshold — proactive wait activates
        rl.remaining = Some(5);
        assert!(rl.is_active().is_some());
    }

    #[test]
    fn combined_status_state_treats_empty_checks_as_pending_when_workflows_exist() {
        assert_eq!(GhHttp::combined_status_state(0, 0, 0, true), "pending");
        assert_eq!(GhHttp::combined_status_state(0, 0, 0, false), "success");
        assert_eq!(GhHttp::combined_status_state(3, 1, 0, true), "failure");
    }

    #[test]
    fn rate_limit_uses_reset_at_for_backoff() {
        let mut rl = make_rl(30, 900);
        // Set reset_at to 45s in the future
        let future_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 45;
        rl.reset_at = Some(future_epoch);
        rl.record_rate_limit();
        // Backoff should use reset_at (~46s) not the base (30s)
        assert!(rl.backoff_delay.as_secs() >= 45);
        assert!(rl.backoff_delay.as_secs() <= 47);
    }

    // ── get_all_pages integration tests (wiremock) ────────────────────────

    #[cfg(test)]
    mod pagination {
        use super::super::*;
        use std::ffi::OsString;
        use std::sync::Arc;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        struct EnvVarGuard {
            key: &'static str,
            previous: Option<OsString>,
        }

        impl EnvVarGuard {
            fn set(key: &'static str, value: &str) -> Self {
                let previous = std::env::var_os(key);
                std::env::set_var(key, value);
                Self { key, previous }
            }
        }

        impl Drop for EnvVarGuard {
            fn drop(&mut self) {
                match self.previous.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }

        /// Build a minimal GhHttp pointing at a wiremock server.
        /// The token resolver reads GH_TOKEN; we set it to a dummy value.
        fn make_client() -> (GhHttp, EnvVarGuard) {
            // Ensure a token is available so auth_header() doesn't bail.
            let guard = EnvVarGuard::set("GH_TOKEN", "test_token");
            let client = GhHttp {
                client: reqwest::Client::new(),
                token_resolver: Arc::new(crate::github::token::TokenResolver::default_env()),
            };
            (client, guard)
        }

        #[tokio::test]
        async fn get_all_pages_single_page() {
            let server = MockServer::start().await;

            Mock::given(method("GET"))
                .and(path("/items"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!([{"id": 1}, {"id": 2}])),
                )
                .expect(1)
                .mount(&server)
                .await;

            let (gh, _guard) = make_client();
            let url = format!("{}/items", server.uri());
            let result: Vec<serde_json::Value> = gh.get_all_pages(&url, &[]).await.unwrap();

            assert_eq!(result.len(), 2);
            assert_eq!(result[0]["id"], 1);
            assert_eq!(result[1]["id"], 2);
        }

        #[tokio::test]
        async fn get_all_pages_follows_link_next() {
            let server = MockServer::start().await;

            // Page 1 — includes Link header pointing to page 2
            Mock::given(method("GET"))
                .and(path("/items"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!([{"id": 1}]))
                        .insert_header(
                            "Link",
                            format!("<{}/items/page2>; rel=\"next\"", server.uri()),
                        ),
                )
                .expect(1)
                .mount(&server)
                .await;

            // Page 2 — no Link header (last page)
            Mock::given(method("GET"))
                .and(path("/items/page2"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!([{"id": 2}, {"id": 3}])),
                )
                .expect(1)
                .mount(&server)
                .await;

            let (gh, _guard) = make_client();
            let url = format!("{}/items", server.uri());
            let result: Vec<serde_json::Value> = gh.get_all_pages(&url, &[]).await.unwrap();

            assert_eq!(result.len(), 3);
            assert_eq!(result[0]["id"], 1);
            assert_eq!(result[1]["id"], 2);
            assert_eq!(result[2]["id"], 3);
        }

        #[tokio::test]
        async fn get_all_pages_stops_when_link_header_absent_on_subsequent_page() {
            // Simulates a proxy/transient response that drops the Link header mid-pagination.
            // The previous `.unwrap()` would panic here; the fix breaks the loop instead.
            let server = MockServer::start().await;

            Mock::given(method("GET"))
                .and(path("/items"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!([{"id": 1}]))
                        .insert_header(
                            "Link",
                            format!("<{}/items/page2>; rel=\"next\"", server.uri()),
                        ),
                )
                .expect(1)
                .mount(&server)
                .await;

            // Page 2 response has NO Link header — pagination should stop cleanly.
            Mock::given(method("GET"))
                .and(path("/items/page2"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!([{"id": 2}])),
                )
                .expect(1)
                .mount(&server)
                .await;

            let (gh, _guard) = make_client();
            let url = format!("{}/items", server.uri());
            let result: Vec<serde_json::Value> = gh.get_all_pages(&url, &[]).await.unwrap();

            // Must not panic and must return both items collected so far.
            assert_eq!(result.len(), 2);
        }

        #[tokio::test]
        async fn get_all_pages_stops_on_malformed_link_header() {
            // A malformed Link header (no angle-bracket URL) yields None from parse_link_next,
            // which should stop pagination rather than panic.
            let server = MockServer::start().await;

            Mock::given(method("GET"))
                .and(path("/items"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!([{"id": 1}]))
                        .insert_header("Link", "garbage; rel=\"next\""),
                )
                .expect(1)
                .mount(&server)
                .await;

            let (gh, _guard) = make_client();
            let url = format!("{}/items", server.uri());
            let result: Vec<serde_json::Value> = gh.get_all_pages(&url, &[]).await.unwrap();

            // Malformed Link header → pagination stops after first page, no panic.
            assert_eq!(result.len(), 1);
        }
    }
}
