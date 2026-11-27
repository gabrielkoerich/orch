//! Native `reqwest` HTTP client for the GitHub API — replaces `gh` CLI subprocesses.
//!
//! Uses a shared `reqwest::Client` with connection pooling, reads rate-limit
//! headers proactively, and supports concurrent requests via `tokio::join!`.
//!
//! Auth: uses the centralized [`TokenResolver`] which supports:
//! - `GH_TOKEN` / `GITHUB_TOKEN` environment variables
//! - `gh.auth.token` config value
//! - `gh auth token` CLI fallback (default — just run `gh auth login`)

use super::token;
use super::types::GitHubUser;
use super::types::{
    GitHubAnnotation, GitHubCheckRun, GitHubComment, GitHubIssue, GitHubPullRequest, GitHubReview,
    GitHubReviewComment, RequiredStatusCheck,
};
use crate::backends::BlockedByFetch;
use crate::backends::BlockedByIssue;
use crate::config;
use crate::engine::cooldown as engine_cooldown;
use crate::engine::cooldown;
use crate::engine::review_gate;
use crate::github::types;
use crate::security;
use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use reqwest::{Client, Response, StatusCode, header};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::any;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::collections::HashSet;
use std::error::Error as _;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::LazyLock;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};
use std::time as std_time;
use std::time::SystemTime;
use std::time::{Duration, Instant};
use tokio::runtime::Handle;
use tokio::time;
use urlencoding;

#[cfg(not(test))]
const GITHUB_API: &str = "https://api.github.com";
// Tests that reach a real GhHttp must fail fast here, never send the operator's token to GitHub
#[cfg(test)]
const GITHUB_API: &str = "http://127.0.0.1:9";

// Writes keep this. GET and GraphQL queries use READ_TIMEOUT so a dead pooled connection fails inside one tick
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
// GET and GraphQL queries, 10 s in production, 1 s in tests so the silent-peer test stays short
const READ_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 10 });
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
// HTTP/2 ping interval, a dead pooled connection fails its ping in about 20 s and leaves the pool
const HTTP2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);
const HTTP2_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, serde::Deserialize)]
struct BlockedByPayload {
    #[serde(default)]
    id: i64,
    number: i64,
    #[serde(default)]
    state: String,
    #[serde(default)]
    state_reason: Option<String>,
    #[serde(default)]
    repository: Option<BlockedByRepoPayload>,
}

#[derive(Debug, serde::Deserialize)]
struct BlockedByRepoPayload {
    full_name: String,
}

pub(crate) fn is_full_commit_sha(sha: &str) -> bool {
    let len = sha.len();
    (len == 40 || len == 64) && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

fn graphql_timeout(query: &str) -> Duration {
    if query.trim_start().starts_with("query") {
        READ_TIMEOUT
    } else {
        REQUEST_TIMEOUT
    }
}

fn github_retry_attempts() -> u32 {
    #[cfg(test)]
    {
        let n = TEST_RETRY_ATTEMPTS.load(Ordering::Relaxed);
        if n > 0 {
            return u32::try_from(n).unwrap_or(3);
        }
    }
    config::get("gh.5xx.retry_attempts")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(3)
}

// Shared gate for every GitHub-posted body so a new call site cannot skip the scan
/// `owner/repo` from a REST URL such as `https://api.github.com/repos/owner/repo/pulls`
fn repo_from_url(url: &str) -> Option<String> {
    let rest = url.split_once("/repos/")?.1;
    let mut parts = rest.split(['/', '?']);
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let name = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{name}"))
}

pub(crate) fn redact_posted_body(body: &str) -> String {
    if !security::has_leaks(body) {
        return body.to_string();
    }
    let leaks = security::scan(body);
    let rules: Vec<&str> = leaks.iter().map(|leak| leak.rule).collect();
    let warning = format!(
        "\n\n> ⚠️ **Security Notice**: {} potential secret(s) detected and redacted: {}",
        leaks.len(),
        rules.join(", ")
    );
    format!("{}{warning}", security::redact(body))
}

fn merge_pr_body(head_sha: &str) -> serde_json::Value {
    serde_json::json!({
        "merge_method": "squash",
        "sha": head_sha,
    })
}

fn parse_positive_issue_number(number: &str) -> anyhow::Result<i64> {
    let parsed = number
        .parse::<i64>()
        .with_context(|| format!("invalid issue number: {number}"))?;
    if parsed <= 0 {
        anyhow::bail!("invalid issue number: {number}");
    }
    Ok(parsed)
}

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
    /// Stop making calls when remaining drops below this threshold (default 200). The
    /// reserve is for the dispatched agents, which run `gh` against the same token.
    throttle_threshold: u32,
}

impl RateLimit {
    fn new() -> Self {
        let base = config::get("gh.backoff.base_seconds")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30);
        let max = config::get("gh.backoff.max_seconds")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(900);
        let threshold = config::get("gh.rate_limit.throttle_threshold")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(200);

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
            match SystemTime::now().duration_since(std_time::UNIX_EPOCH) {
                Ok(since_epoch) => {
                    let now_epoch = since_epoch.as_secs();
                    if reset_epoch > now_epoch {
                        let wait_secs = reset_epoch - now_epoch + 1;
                        self.backoff_delay = Duration::from_secs(wait_secs);
                        self.backoff_until = Some(Instant::now() + self.backoff_delay);
                        tracing::warn!(
                            wait_secs,
                            "GitHub rate limit hit, waiting until reset time"
                        );
                        return;
                    }
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "System clock is before UNIX epoch; ignoring reset_at and falling back to exponential backoff"
                    );
                }
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

    /// Returns how long until the window resets when the quota is under the reserve.
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
        if let Some(remaining) = self.remaining
            && remaining < self.throttle_threshold
            && let Some(reset_epoch) = self.reset_at
        {
            let now_epoch = SystemTime::now()
                .duration_since(std_time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if now_epoch < reset_epoch {
                return Some(Duration::from_secs(reset_epoch - now_epoch + 1));
            }
        }
        None
    }

    /// Combined check: backoff or proactive throttle. Used by engine's is_rate_limited().
    fn is_active(&self) -> Option<Duration> {
        self.backoff_remaining()
            .or_else(|| self.proactive_wait_duration())
    }
}

// ── Global state ─────────────────────────────────────────────────────

static REST_RATE_LIMIT: LazyLock<Mutex<RateLimit>> = LazyLock::new(|| Mutex::new(RateLimit::new()));

#[cfg(test)]
static TEST_RETRY_ATTEMPTS: AtomicU64 = AtomicU64::new(0);

/// Separate rate-limit tracker for the GraphQL endpoint (distinct quota from REST).
static GRAPHQL_RATE_LIMIT: LazyLock<Mutex<RateLimit>> =
    LazyLock::new(|| Mutex::new(RateLimit::new()));

// ── Rate-limit metrics ────────────────────────────────────────────────

/// Total 403/429 rate-limit responses received (REST + GraphQL).
static METRIC_RATE_LIMIT_HITS: AtomicU64 = AtomicU64::new(0);
/// Calls skipped because the quota was under the reserve threshold or a backoff was active.
static METRIC_PROACTIVE_THROTTLES: AtomicU64 = AtomicU64::new(0);
/// Total 5xx server error responses received (REST + GraphQL).
static METRIC_5XX_HITS: AtomicU64 = AtomicU64::new(0);

// ── Rate-limit generic helpers ────────────────────────────────────────

/**
 * Fail fast when the limiter is in hard backoff (post-403/429) or the remaining quota is
 * under the reserve threshold. This must never sleep: the engine runs every tick phase
 * through these calls, so waiting out a reset here froze the whole tick loop for up to an
 * hour and tripped the watchdog. Returning an error lets the phase give up and the tick
 * finish, and the next tick retries once the window resets
 **/
fn check_rate_limit_backoff(cell: &Mutex<RateLimit>, api: &str) -> anyhow::Result<()> {
    let Some(remaining) = cell.lock().ok().and_then(|rl| rl.is_active()) else {
        return Ok(());
    };
    METRIC_PROACTIVE_THROTTLES.fetch_add(1, Ordering::Relaxed);
    anyhow::bail!(
        "GitHub {} API rate-limited, skipping call for {}s",
        api,
        remaining.as_secs()
    );
}

/// API calls since the last drain, keyed by (project, call type), for the per-tick debug report.
static CALL_COUNTS: LazyLock<Mutex<HashMap<(String, String), u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Project is `owner/repo` from a `/repos/` URL and call type is the path segment after it
/// (`pulls`, `issues`, `check-runs`), so non-repo endpoints land under `-`.
fn count_api_call(url: &str) {
    let path = url.split('?').next().unwrap_or(url);
    let key = match path.split_once("/repos/") {
        Some((_, rest)) => {
            let mut parts = rest.split('/');
            let project = format!(
                "{}/{}",
                parts.next().unwrap_or(""),
                parts.next().unwrap_or("")
            );
            (project, parts.next().unwrap_or("repo").to_string())
        }
        None if path.ends_with("/graphql") => ("-".to_string(), "graphql".to_string()),
        None => ("-".to_string(), "other".to_string()),
    };
    if let Ok(mut counts) = CALL_COUNTS.lock() {
        *counts.entry(key).or_insert(0) += 1;
    }
}

/// Take the call counts accumulated since the last drain, largest first.
pub fn drain_call_counts() -> Vec<(String, String, u64)> {
    let mut rows: Vec<_> = CALL_COUNTS
        .lock()
        .map(|mut c| c.drain().map(|((p, k), n)| (p, k, n)).collect())
        .unwrap_or_default();
    rows.sort_by_key(|r| Reverse(r.2));
    rows
}

// ── Conditional-request cache ─────────────────────────────────────────

/// A stored 200 response, replayed when GitHub answers the next conditional GET with 304.
struct CachedPage {
    etag: String,
    body: String,
    /// `Link: rel="next"` from the cached response; a 304 does not repeat it.
    next: Option<String>,
}

/**
 * Keyed by the full request URL plus the decoded type, since the body is replayed through
 * `serde_json` and two call sites can share a URL while wanting different shapes. GitHub
 * answers a conditional GET with 304 and no body when nothing changed, and that response
 * does not count against the core quota, so the listing endpoints orch polls every tick
 * cost one request per hour of quiet instead of one per tick
 **/
static PAGE_CACHE: LazyLock<Mutex<HashMap<String, CachedPage>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Distinct cacheable list URLs are few per repo, plus one blocked-by URL per open issue.
/// The cap only guards a leak. Each entry holds a full page body, so keep it small
const PAGE_CACHE_MAX: usize = 256;

/// `None` when the request cannot produce a hit, so it is neither sent conditionally nor
/// stored. A `since` filter is computed from the clock on every call, so its key never
/// recurs and caching it would only grow the map with page-sized bodies.
fn page_cache_key<T>(url: &str, query: Option<&[(&str, &str)]>) -> Option<String> {
    let mut key = String::from(any::type_name::<T>());
    key.push(' ');
    key.push_str(url);
    if let Some(q) = query {
        for (i, (k, v)) in q.iter().enumerate() {
            key.push(if i == 0 { '?' } else { '&' });
            key.push_str(k);
            key.push('=');
            key.push_str(v);
        }
    }
    if key.contains("since=") {
        return None;
    }
    Some(key)
}

fn page_cache_etag(key: &str) -> Option<String> {
    PAGE_CACHE.lock().ok()?.get(key).map(|c| c.etag.clone())
}

/// Returns the stored body and `next` link for a 304 reply.
fn page_cache_hit(key: &str) -> Option<(String, Option<String>)> {
    let cache = PAGE_CACHE.lock().ok()?;
    let entry = cache.get(key)?;
    Some((entry.body.clone(), entry.next.clone()))
}

fn page_cache_store(key: String, etag: String, body: String, next: Option<String>) {
    let Ok(mut cache) = PAGE_CACHE.lock() else {
        return;
    };
    // ponytail: clears everything at the cap instead of evicting one entry, switch to an
    // LRU if the hit rate ever matters more than the bookkeeping
    if cache.len() >= PAGE_CACHE_MAX && !cache.contains_key(&key) {
        cache.clear();
    }
    cache.insert(key, CachedPage { etag, body, next });
}

/// Check response body for rate-limit signals on 403 responses.
/// Not all 403s are rate limits — some are permission errors.
fn maybe_record_body_rate_limit(cell: &Mutex<RateLimit>, status: StatusCode, body: &str) {
    if is_forbidden_rate_limit(status, body)
        && let Ok(mut rl) = cell.lock()
    {
        rl.record_rate_limit();
    }
}

/// True for a 403 whose body names a primary or secondary rate limit
fn is_forbidden_rate_limit(status: StatusCode, body: &str) -> bool {
    if status != StatusCode::FORBIDDEN {
        return false;
    }
    let lower = body.to_lowercase();
    lower.contains("rate limit")
        || lower.contains("abuse detection")
        || lower.contains("secondary rate")
}

/// Update rate-limit state from response headers/status and detect 5xx errors.
fn record_response_for_limiter(cell: &Mutex<RateLimit>, resp: &Response) {
    if let Ok(mut rl) = cell.lock() {
        rl.update_from_headers(resp.headers());
        // Only backoff on 429 (always rate limit) — 403 requires body inspection
        // which is handled in maybe_record_body_rate_limit().
        if resp.status() == StatusCode::TOO_MANY_REQUESTS {
            rl.record_rate_limit();
        } else if resp.status().is_success() || resp.status() == StatusCode::NOT_MODIFIED {
            rl.record_success();
        }
    }
    // Detect 5xx server errors for the global circuit breaker.
    let status = resp.status();
    if status.is_server_error() {
        METRIC_5XX_HITS.fetch_add(1, Ordering::Relaxed);
        if Handle::try_current().is_ok() {
            tokio::spawn(async move {
                cooldown::record_github_5xx().await;
            });
        }
    }
}

// ── Shared token resolver ────────────────────────────────────────────

// ── GhHttp client ────────────────────────────────────────────────────

/// Native HTTP client for the GitHub API with connection pooling and
/// proactive rate-limit avoidance.
#[derive(Clone)]
pub struct GhHttp {
    client: Client,
    token_resolver: Arc<token::TokenResolver>,
    // An App only tests set, production calls always use the gh token
    app: Option<Arc<super::app::AppAuth>>,
    // Calls the App cannot make, such as `GET /user` and Projects V2 on a personal account
    owner_only: bool,
    cached_username: OnceLock<String>,
    #[cfg(test)]
    api_base: Option<String>,
}

impl Debug for GhHttp {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("GhHttp")
            .field("owner_only", &self.owner_only)
            .finish_non_exhaustive()
    }
}

/// The error with its full source chain plus the timeout and connect flags, because the
/// top-level reqwest message alone hides why the request failed
fn transport_error_detail(e: &reqwest::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let _ = write!(out, ": {s}");
        source = s.source();
    }
    let _ = write!(
        out,
        " (is_timeout={}, is_connect={})",
        e.is_timeout(),
        e.is_connect()
    );
    out
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
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .tcp_keepalive(TCP_KEEPALIVE)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .http2_keep_alive_interval(HTTP2_KEEP_ALIVE_INTERVAL)
            .http2_keep_alive_timeout(HTTP2_KEEP_ALIVE_TIMEOUT)
            .http2_keep_alive_while_idle(true)
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build HTTP client (TLS init): {e}"))?;

        Ok(Self {
            client,
            token_resolver: token::shared(),
            app: None,
            owner_only: false,
            cached_username: OnceLock::new(),
            #[cfg(test)]
            api_base: None,
        })
    }

    /// A client that always uses the owner token, for calls the App cannot make, such as
    /// `GET /user` and Projects V2 on a personal account
    pub fn owner() -> anyhow::Result<Self> {
        let mut gh = Self::new()?;
        gh.owner_only = true;
        Ok(gh)
    }

    #[cfg(test)]
    pub(crate) fn for_test(api_base: String, app: Option<Arc<super::app::AppAuth>>) -> Self {
        Self {
            client: Client::new(),
            token_resolver: Arc::new(token::TokenResolver::default_env()),
            app,
            owner_only: false,
            cached_username: OnceLock::new(),
            api_base: Some(api_base),
        }
    }

    #[cfg_attr(
        not(test),
        expect(
            clippy::unused_self,
            reason = "only the test build reads the api base override"
        )
    )]
    fn api_root(&self) -> &str {
        #[cfg(test)]
        if let Some(base) = &self.api_base {
            return base;
        }
        GITHUB_API
    }

    // ── Rate-limit helpers ────────────────────────────────────────

    /// Check if the REST GitHub API is currently rate-limited (backoff or approaching limit).
    /// Used by the engine to decide whether to skip a tick.
    pub fn is_rate_limited() -> Option<Duration> {
        REST_RATE_LIMIT.lock().ok().and_then(|rl| rl.is_active())
    }

    /// Fail fast if the REST API is in hard backoff (post-403/429).
    fn check_backoff(url: &str) -> anyhow::Result<()> {
        count_api_call(url);
        check_rate_limit_backoff(&REST_RATE_LIMIT, "REST")
    }

    /// Fail fast if the GraphQL API is in hard backoff (post-403/429).
    fn check_graphql_backoff(url: &str) -> anyhow::Result<()> {
        count_api_call(url);
        check_rate_limit_backoff(&GRAPHQL_RATE_LIMIT, "GraphQL")
    }

    fn record_response(resp: &Response) {
        record_response_for_limiter(&REST_RATE_LIMIT, resp);
    }

    fn record_graphql_response(resp: &Response) {
        record_response_for_limiter(&GRAPHQL_RATE_LIMIT, resp);
    }

    /// Check response body for rate-limit signals on 403 responses (REST).
    fn maybe_record_rate_limit_from_body(status: StatusCode, body: &str) {
        maybe_record_body_rate_limit(&REST_RATE_LIMIT, status, body);
    }

    /// Check response body for rate-limit signals on 403 responses (GraphQL).
    fn maybe_record_graphql_rate_limit_from_body(status: StatusCode, body: &str) {
        maybe_record_body_rate_limit(&GRAPHQL_RATE_LIMIT, status, body);
    }

    fn record_success() {
        if let Ok(mut rl) = REST_RATE_LIMIT.lock() {
            rl.record_success();
        }
    }

    // ── Low-level HTTP helpers ────────────────────────────────────

    fn app(&self) -> Option<Arc<super::app::AppAuth>> {
        if self.owner_only {
            return None;
        }

        // orch's own calls use the gh token, the App only reviews (src/engine/review.rs). The App
        // must not open PRs, GitHub refuses a review from a PR's author
        self.app.clone()
    }

    /// The App's token for a repository it is installed on, the owner token for anything else,
    /// including GraphQL, whose URL names no repository
    async fn auth_header(&self, url: &str) -> anyhow::Result<String> {
        if let (Some(app), Some(repo)) = (self.app(), repo_from_url(url)) {
            match app.token_for_repo(&repo).await {
                Ok(Some(token)) => return Ok(format!("Bearer {token}")),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error = %e, repo, "GitHub App token unavailable, using the owner token");
                }
            }
        }
        match self.token_resolver.get_token().await? {
            Some(token) if !token.is_empty() => Ok(format!("Bearer {token}")),
            _ => anyhow::bail!("No GitHub token available — run `gh auth login` or set GH_TOKEN"),
        }
    }

    /// Generic send wrapper that retries on 5xx server errors with exponential
    /// backoff and sets a short-lived circuit-breaker in KV when retries fail.
    async fn send_with_retries<F>(
        &self,
        mut make_req: F,
        is_graphql: bool,
    ) -> anyhow::Result<Response>
    where
        F: FnMut() -> anyhow::Result<reqwest::RequestBuilder>,
    {
        // Circuit-breaker: if set, fail fast to avoid wasting work.
        if engine_cooldown::is_agent_in_cooldown("github:5xx") {
            if let Some(until) = engine_cooldown::cooldown_until("github:5xx") {
                let now = chrono::Utc::now().timestamp();
                let remaining = if until > now { until - now } else { 0 };
                anyhow::bail!("GitHub API transient 5xx circuit-breaker active for {remaining}s");
            }
            anyhow::bail!("GitHub API transient 5xx circuit-breaker active");
        }

        // Configurable retry params (defaults chosen conservatively)
        let attempts = github_retry_attempts();
        let base_secs = config::get("gh.5xx.base_backoff_seconds")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(1);
        let max_secs = config::get("gh.5xx.max_backoff_seconds")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30);
        let circuit_cooldown_secs = config::get("gh.5xx.circuit_breaker_cooldown_seconds")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(300);

        let mut last_err: Option<anyhow::Error> = None;
        // A cached token can go bad mid-process (revoked, rotated, stale keychain read).
        // On the first 401 Bad credentials: evict the cache, re-resolve once, resend
        let mut auth_override: Option<String> = None;
        let mut auth_retried = false;
        for attempt in 0..attempts {
            let mut req = make_req()?;
            if let Some(auth) = &auth_override {
                req = req.header(header::AUTHORIZATION, auth.clone());
            }
            let resp_result = req.send().await;

            match resp_result {
                Ok(resp) => {
                    // Record rate-limit state depending on REST vs GraphQL
                    if is_graphql {
                        Self::record_graphql_response(&resp);
                    } else {
                        Self::record_response(&resp);
                    }

                    if resp.status() == StatusCode::UNAUTHORIZED {
                        let resp_url = resp.url().to_string();
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        if body.contains("Bad credentials") && !auth_retried {
                            tracing::warn!(
                                "GitHub API 401 Bad credentials — clearing cached token and re-resolving"
                            );
                            self.token_resolver.clear_cache().await;
                            if let Some(app) = self.app() {
                                app.invalidate();
                            }
                            match self.auth_header(&resp_url).await {
                                Ok(new_auth) => {
                                    auth_retried = true;
                                    auth_override = Some(new_auth);
                                }
                                Err(e) => {
                                    last_err = Some(
                                        e.context("GitHub token re-resolution failed after 401"),
                                    );
                                    break;
                                }
                            }
                            // ponytail: consumes one 5xx-retry slot, attempts default is 3
                            continue;
                        }
                        if auth_retried {
                            // Fresh token also rejected; leave the cache empty so the next
                            // call re-resolves and picks up env/config fixes immediately
                            self.token_resolver.clear_cache().await;
                            anyhow::bail!(
                                "GitHub authentication failed ({status}): token re-resolved after cache clear but still rejected"
                            );
                        }
                        anyhow::bail!("GitHub API request failed ({status}): {body}");
                    }

                    // If server error, treat as transient and retry
                    if resp.status().is_server_error() {
                        let status = resp.status();
                        let headers = resp.headers().clone();
                        let body = resp.text().await.unwrap_or_default();
                        let req_id = headers
                            .get("x-github-request-id")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("<none>");
                        let snippet: String = body.chars().take(400).collect();
                        tracing::warn!(
                            status = %status,
                            request_id = %req_id,
                            attempt,
                            "GitHub server error (5xx) — attempt {attempt}: {snippet}"
                        );

                        last_err = Some(anyhow::anyhow!(
                            "GitHub API server error ({status}) request_id={req_id}: {snippet}"
                        ));

                        if attempt + 1 < attempts {
                            // Exponential backoff (2^attempt * base), capped
                            // Cap exponent at 63 (max valid shift for u64) to prevent overflow
                            let exp = 1u64.checked_shl(attempt.min(63)).unwrap_or(u64::MAX);
                            let mut backoff = base_secs.saturating_mul(exp);
                            if backoff > max_secs {
                                backoff = max_secs;
                            }
                            // Add small linear jitter to avoid synchronized retries
                            let jitter_ms = u64::from(attempt) * 100;
                            let sleep_dur = Duration::from_secs(backoff)
                                + Duration::from_millis(jitter_ms.min(1000));
                            time::sleep(sleep_dur).await;
                            continue;
                        }
                        // Exhausted attempts — set circuit-breaker and break so last_err is read
                        tracing::warn!(
                            "GitHub API 5xx persisted after {} attempts — setting circuit-breaker for {}s",
                            attempts,
                            circuit_cooldown_secs
                        );
                        engine_cooldown::set_agent_cooldown("github:5xx", circuit_cooldown_secs)
                            .await;
                        break;
                    }

                    // Non-5xx response — return it for caller to handle success/failure
                    return Ok(resp);
                }
                Err(e) => {
                    // Transport error — the request never reached GitHub (DNS failure,
                    // connection refused, no route to host, TLS failure, etc). This is a
                    // local/network condition, not evidence that GitHub itself is unhealthy,
                    // so unlike the 5xx arm above it must NOT trip the shared "github:5xx"
                    // circuit breaker: other subsystems (routing-phase skip, cooldown
                    // throttling) key off that breaker as a signal about GitHub's health.
                    let detail = transport_error_detail(&e);
                    tracing::warn!(err = %detail, attempt, "HTTP request failed to send (transport error, GitHub not reached), will retry if attempts remain");
                    last_err = Some(anyhow::anyhow!(
                        "HTTP request failed to send (transport error): {detail}"
                    ));
                    if attempt + 1 < attempts {
                        // Cap exponent at 63 (max valid shift for u64) to prevent overflow
                        let exp = 1u64.checked_shl(attempt.min(63)).unwrap_or(u64::MAX);
                        let backoff = base_secs.saturating_mul(exp);
                        let sleep_dur = Duration::from_secs(backoff)
                            + Duration::from_millis((u64::from(attempt) * 100).min(1000));
                        time::sleep(sleep_dur).await;
                        continue;
                    }
                    // Exhausted attempts — break so last_err is read. Deliberately does not
                    // call set_agent_cooldown("github:5xx", ...): GitHub was never reached,
                    // so its health is unknown and the shared breaker must stay untouched.
                    tracing::warn!(
                        "HTTP request failed to send after {} attempts (transport error, GitHub not reached) — not tripping the github:5xx circuit-breaker",
                        attempts
                    );
                    break;
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("unexpected send_with_retries failure")))
    }

    /// GET request, returns deserialized JSON.
    async fn get_json<T: DeserializeOwned>(&self, url: &str) -> anyhow::Result<T> {
        Self::check_backoff(url)?;
        let auth = self.auth_header(url).await?;
        // Use send_with_retries to handle transient 5xx/network failures
        let make_req = || {
            Ok(self
                .client
                .get(url)
                .timeout(READ_TIMEOUT)
                .header(header::AUTHORIZATION, auth.clone())
                .header(header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"))
        };
        let resp = self.send_with_retries(make_req, false).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            Self::maybe_record_rate_limit_from_body(status, &body);
            anyhow::bail!("GitHub API GET {url} failed ({status}): {body}");
        }
        let body = resp.text().await?;
        serde_json::from_str(&body)
            .with_context(|| format!("failed to parse GitHub API response from GET {url}: {body}"))
    }

    /// GET raw bytes (for endpoints that return non-JSON or we parse manually).
    async fn get_bytes(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        Self::check_backoff(url)?;
        let auth = self.auth_header(url).await?;
        let make_req = || {
            Ok(self
                .client
                .get(url)
                .timeout(READ_TIMEOUT)
                .header(header::AUTHORIZATION, auth.clone())
                .header(header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"))
        };
        let resp = self.send_with_retries(make_req, false).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            Self::maybe_record_rate_limit_from_body(status, &body);
            anyhow::bail!("GitHub API GET {url} failed ({status}): {body}");
        }
        Ok(resp.bytes().await?.to_vec())
    }

    /// GET with query params, returns deserialized JSON.
    async fn get_with_query<T: DeserializeOwned>(
        &self,
        url: &str,
        query: &[(&str, &str)],
    ) -> anyhow::Result<T> {
        Self::check_backoff(url)?;
        let auth = self.auth_header(url).await?;
        let make_req = || {
            Ok(self
                .client
                .get(url)
                .timeout(READ_TIMEOUT)
                .query(query)
                .header(header::AUTHORIZATION, auth.clone())
                .header(header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"))
        };
        let resp = self.send_with_retries(make_req, false).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            Self::maybe_record_rate_limit_from_body(status, &body);
            anyhow::bail!("GitHub API GET {url} failed ({status}): {body}");
        }
        let body = resp.text().await?;
        serde_json::from_str(&body)
            .with_context(|| format!("failed to parse GitHub API response from GET {url}: {body}"))
    }

    /// POST with JSON body, returns raw response text.
    async fn post_json_raw(&self, url: &str, body: &serde_json::Value) -> anyhow::Result<String> {
        Self::check_backoff(url)?;
        let auth = self.auth_header(url).await?;
        let make_req = || {
            Ok(self
                .client
                .post(url)
                .json(body)
                .header(header::AUTHORIZATION, auth.clone())
                .header(header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"))
        };
        let resp = self.send_with_retries(make_req, false).await?;
        let status = resp.status();
        let text_res = resp.text().await;
        if !status.is_success() {
            let text = text_res.unwrap_or_default();
            Self::maybe_record_rate_limit_from_body(status, &text);
            anyhow::bail!("GitHub API POST {url} failed ({status}): {text}");
        }
        let text = text_res?;
        Ok(text)
    }

    /// POST with JSON body, returns deserialized JSON.
    async fn post_json<T: DeserializeOwned>(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> anyhow::Result<T> {
        let text = self.post_json_raw(url, body).await?;
        Ok(serde_json::from_str(&text)?)
    }

    /// PATCH with JSON body, returns raw response.
    async fn patch_json_raw(&self, url: &str, body: &serde_json::Value) -> anyhow::Result<String> {
        Self::check_backoff(url)?;
        let auth = self.auth_header(url).await?;
        let make_req = || {
            Ok(self
                .client
                .patch(url)
                .json(body)
                .header(header::AUTHORIZATION, auth.clone())
                .header(header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"))
        };
        let resp = self.send_with_retries(make_req, false).await?;
        let status = resp.status();
        let text_res = resp.text().await;
        if !status.is_success() {
            let text = text_res.unwrap_or_default();
            Self::maybe_record_rate_limit_from_body(status, &text);
            anyhow::bail!("GitHub API PATCH {url} failed ({status}): {text}");
        }
        let text = text_res?;
        Ok(text)
    }

    /// Create or replace one file on a branch with a commit, the way the GitHub web editor does
    pub async fn put_file(
        &self,
        repo: &str,
        path: &str,
        branch: &str,
        message: &str,
        content: &str,
    ) -> anyhow::Result<()> {
        let encoded_path = path
            .split('/')
            .map(|segment| urlencoding::encode(segment).into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let url = format!("{}/repos/{repo}/contents/{encoded_path}", self.api_root());
        // Replacing a file needs the blob it replaces, a new file has none
        let current = self
            .get_json::<serde_json::Value>(&format!("{url}?ref={}", urlencoding::encode(branch)))
            .await
            .ok()
            .and_then(|v| v.get("sha").and_then(|s| s.as_str()).map(str::to_string));
        let mut body = serde_json::json!({
            "message": message,
            "content": BASE64.encode(content),
            "branch": branch,
        });
        if let Some(sha) = current {
            body["sha"] = serde_json::Value::String(sha);
        }
        self.put_json_raw(&url, &body).await.map(|_| ())
    }

    /// PUT with JSON body, returns raw response.
    async fn put_json_raw(&self, url: &str, body: &serde_json::Value) -> anyhow::Result<String> {
        Self::check_backoff(url)?;
        let auth = self.auth_header(url).await?;
        let make_req = || {
            Ok(self
                .client
                .put(url)
                .json(body)
                .header(header::AUTHORIZATION, auth.clone())
                .header(header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"))
        };
        let resp = self.send_with_retries(make_req, false).await?;
        let status = resp.status();
        let text_res = resp.text().await;
        if !status.is_success() {
            let text = text_res.unwrap_or_default();
            Self::maybe_record_rate_limit_from_body(status, &text);
            anyhow::bail!("GitHub API PUT {url} failed ({status}): {text}");
        }
        let text = text_res?;
        Ok(text)
    }

    /// DELETE request.
    async fn delete(&self, url: &str) -> anyhow::Result<StatusCode> {
        Self::check_backoff(url)?;
        let auth = self.auth_header(url).await?;
        let make_req = || {
            Ok(self
                .client
                .delete(url)
                .header(header::AUTHORIZATION, auth.clone())
                .header(header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"))
        };
        let resp = self.send_with_retries(make_req, false).await?;
        let status = resp.status();
        if !status.is_success() && status != StatusCode::NOT_FOUND {
            let body = resp.text().await.unwrap_or_default();
            Self::maybe_record_rate_limit_from_body(status, &body);
            anyhow::bail!("GitHub API DELETE {url} failed ({status}): {body}");
        }
        Ok(status)
    }

    /// Paginated GET — follows Link: <next> headers, with conditional requests.
    ///
    /// Each page is sent with `If-None-Match` when a prior ETag is known; GitHub's 304
    /// reply is free of quota and the stored body is replayed.
    async fn get_all_pages<T: DeserializeOwned>(
        &self,
        url: &str,
        query: &[(&str, &str)],
    ) -> anyhow::Result<Vec<T>> {
        const MAX_JSON_DECODE_RETRIES: u32 = 2;
        const JSON_DECODE_RETRY_BACKOFF_MS: u64 = 200;
        Self::check_backoff(url)?;
        let mut all: Vec<T> = Vec::new();
        let mut next_url: Option<String> = None;
        let mut is_first = true;

        loop {
            let (request_url, request_query) = if is_first {
                is_first = false;
                (url, Some(query))
            } else {
                let Some(u) = next_url.as_ref() else { break };
                (u.as_str(), None)
            };

            let cache_key = page_cache_key::<T>(request_url, request_query);
            let mut cached_etag = cache_key.as_deref().and_then(page_cache_etag);
            let mut decode_attempt = 0;
            let (page, next) = loop {
                let auth = self.auth_header(request_url).await?;
                let mut req = self
                    .client
                    .get(request_url)
                    .timeout(READ_TIMEOUT)
                    .header(header::AUTHORIZATION, auth)
                    .header(header::ACCEPT, "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28");
                if let Some(q) = request_query {
                    req = req.query(q);
                }
                if let Some(etag) = &cached_etag {
                    req = req.header(header::IF_NONE_MATCH, etag.clone());
                }

                let make_req = || {
                    req.try_clone()
                        .ok_or_else(|| anyhow::anyhow!("request clone failed"))
                };
                let resp = self.send_with_retries(make_req, false).await?;
                let status = resp.status();
                let next_from_headers = parse_link_next(resp.headers());
                let etag = resp
                    .headers()
                    .get(header::ETAG)
                    .and_then(|v| v.to_str().ok())
                    .map(ToString::to_string);
                let content_type = resp
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map_or_else(|| "<missing>".to_string(), ToString::to_string);

                if status == StatusCode::NOT_MODIFIED {
                    if let Some((body, next)) = cache_key.as_deref().and_then(page_cache_hit) {
                        break (serde_json::from_str::<Vec<T>>(&body)?, next);
                    }
                    // The entry is normally there, since If-None-Match is only sent
                    // when it is, but the size cap can clear the map in between.
                    // Dropping the ETag makes the retry unconditional, so the loop
                    // cannot see a second 304
                    cached_etag = None;
                    continue;
                }

                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    Self::maybe_record_rate_limit_from_body(status, &body);
                    anyhow::bail!("GitHub API GET (paginated) failed ({status}): {body}");
                }

                let text = resp.text().await?;
                match serde_json::from_str::<Vec<T>>(&text) {
                    Ok(page) => {
                        if let (Some(key), Some(etag)) = (cache_key.clone(), etag) {
                            page_cache_store(key, etag, text, next_from_headers.clone());
                        }
                        break (page, next_from_headers);
                    }
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
                            time::sleep(Duration::from_millis(
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

    /// Paginated GET for GitHub endpoints that return a wrapper object instead of a raw
    /// array.  Each page is expected to look like `{ "<array_field>": [...], ... }`.
    /// The function extracts `array_field` from every page and appends the elements
    /// to the accumulated result, following `Link: <next>` headers until exhausted.
    ///
    /// This is necessary for endpoints such as `/commits/{sha}/check-runs` which
    /// return `{ "total_count": N, "check_runs": [...] }` rather than a bare `[...]`
    /// array.  Using `get_all_pages` on those endpoints would fail at the
    /// `serde_json::from_str::<Vec<T>>` step because the body is an object, not an
    /// array.
    async fn get_all_pages_from_wrapper<T: DeserializeOwned>(
        &self,
        url: &str,
        array_field: &str,
    ) -> anyhow::Result<Vec<T>> {
        Self::check_backoff(url)?;
        let mut all: Vec<T> = Vec::new();
        let mut current_url: String = url.to_string();

        loop {
            let auth = self.auth_header(&current_url).await?;
            let make_req = || {
                self.client
                    .get(&current_url)
                    .timeout(READ_TIMEOUT)
                    .header(header::AUTHORIZATION, auth.clone())
                    .header(header::ACCEPT, "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28")
                    .try_clone()
                    .ok_or_else(|| anyhow::anyhow!("request clone failed"))
            };
            let resp = self.send_with_retries(make_req, false).await?;
            let status = resp.status();
            let next = parse_link_next(resp.headers());

            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                Self::maybe_record_rate_limit_from_body(status, &body);
                anyhow::bail!(
                    "GitHub API GET (wrapped paginated) {current_url} failed ({status}): {body}"
                );
            }

            let body = resp.text().await?;
            let wrapper: serde_json::Value = serde_json::from_str(&body).with_context(|| {
                format!("failed to parse wrapper response from {current_url}: {body}")
            })?;
            let items = wrapper
                .get(array_field)
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            for item in items {
                match serde_json::from_value::<T>(item) {
                    Ok(v) => all.push(v),
                    Err(e) => {
                        tracing::warn!(
                            field = array_field,
                            err = %e,
                            "skipping item that failed to deserialize in wrapped pagination"
                        );
                    }
                }
            }

            match next {
                Some(next_url) => current_url = next_url,
                None => break,
            }
        }

        Ok(all)
    }

    /// POST to the GraphQL endpoint. Uses separate rate-limit tracking from REST.
    async fn graphql_request(
        &self,
        query: &str,
        extra_headers: &[(&str, &str)],
        variables: Option<serde_json::Value>,
    ) -> anyhow::Result<serde_json::Value> {
        self.graphql_request_to_url(
            &format!("{GITHUB_API}/graphql"),
            query,
            extra_headers,
            variables,
        )
        .await
    }

    async fn graphql_request_to_url(
        &self,
        url: &str,
        query: &str,
        extra_headers: &[(&str, &str)],
        variables: Option<serde_json::Value>,
    ) -> anyhow::Result<serde_json::Value> {
        Self::check_graphql_backoff(url)?;
        let body = if let Some(vars) = variables {
            serde_json::json!({ "query": query, "variables": vars })
        } else {
            serde_json::json!({ "query": query })
        };
        let auth = self.auth_header(url).await?;
        let mut req = self
            .client
            .post(url)
            .timeout(graphql_timeout(query))
            .json(&body)
            .header(header::AUTHORIZATION, auth)
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");

        for (k, v) in extra_headers {
            req = req.header(*k, *v);
        }

        // Use send_with_retries wrapper for GraphQL (is_graphql = true)
        let make_req = || {
            req.try_clone()
                .ok_or_else(|| anyhow::anyhow!("graphql request clone failed"))
        };
        let resp = self.send_with_retries(make_req, true).await?;
        let status = resp.status();
        let text_res = resp.text().await;
        if !status.is_success() {
            let text = text_res.unwrap_or_default();
            Self::maybe_record_graphql_rate_limit_from_body(status, &text);
            anyhow::bail!("GitHub GraphQL failed ({status}): {text}");
        }
        let text = text_res?;
        let json: serde_json::Value = serde_json::from_str(&text)?;
        // GraphQL always returns 200 even for errors; check the errors field explicitly.
        if let Some(errors) = json.get("errors").and_then(|e| e.as_array())
            && !errors.is_empty()
        {
            let messages: Vec<&str> = errors
                .iter()
                .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                .collect();
            if json.get("data").is_none() || json["data"].is_null() {
                // No data at all — surface the errors as a proper error.
                anyhow::bail!("GitHub GraphQL errors: {}", messages.join("; "));
            }
            // Partial data alongside errors — log and continue.
            tracing::warn!(errors = ?messages, "GitHub GraphQL returned partial errors");
        }
        Ok(json)
    }

    // ── Public API (mirrors GhCli) ───────────────────────────────

    pub async fn graphql_with_vars(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.graphql_request(query, &[], Some(variables)).await
    }

    pub async fn graphql_with_headers(
        &self,
        query: &str,
        headers: &[&str],
    ) -> anyhow::Result<serde_json::Value> {
        // Convert "Key:Value" strings to (&str, &str) tuples.
        let pairs: Vec<(&str, &str)> = headers.iter().filter_map(|h| h.split_once(':')).collect();
        self.graphql_request(query, &pairs, None).await
    }

    pub async fn graphql_with_headers_and_vars(
        &self,
        query: &str,
        headers: &[&str],
        variables: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let pairs: Vec<(&str, &str)> = headers.iter().filter_map(|h| h.split_once(':')).collect();
        self.graphql_request(query, &pairs, Some(variables)).await
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

    /// Replace an issue's title, body and labels.
    ///
    /// Labels replace the full set. Callers pass labels they want to keep
    pub async fn update_issue(
        &self,
        repo: &str,
        number: u64,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> anyhow::Result<GitHubIssue> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues/{number}");
        let payload = serde_json::json!({
            "title": title,
            "body": body,
            "labels": labels,
        });
        let text = self.patch_json_raw(&url, &payload).await?;
        serde_json::from_str(&text)
            .with_context(|| format!("failed to parse issue update response: {text}"))
    }

    /// List issues that block `issue_number`.
    ///
    /// `GET /repos/{owner}/{repo}/issues/{issue_number}/dependencies/blocked_by`.
    /// GA since 2025-08-21, no preview media type. This call sends
    /// `X-GitHub-Api-Version: 2026-03-10` and `Accept: application/vnd.github+json`.
    /// Each page is sent with `If-None-Match` when a prior ETag is known, and a 304
    /// replays the stored body. 404 and 403 (except rate limit) are `Unavailable`.
    /// Transport errors and 5xx are `Failed`, so the caller does not drop stored links
    pub async fn list_blocked_by(
        &self,
        repo: &str,
        issue_number: u64,
    ) -> anyhow::Result<BlockedByFetch> {
        let mut next = Some(format!(
            "{}/repos/{repo}/issues/{issue_number}/dependencies/blocked_by?per_page=100",
            self.api_root()
        ));
        let mut found = Vec::new();
        while let Some(url) = next.take() {
            let cache_key = page_cache_key::<BlockedByPayload>(&url, None);
            let mut cached_etag = cache_key.as_deref().and_then(page_cache_etag);
            let (body, page_next) = loop {
                let resp = match self
                    .dependency_request(&url, reqwest::Method::GET, None, cached_etag.as_deref())
                    .await
                {
                    Ok(resp) => resp,
                    Err(err) => {
                        let message = err.to_string();
                        // `send_with_retries` words a send failure with this marker
                        if message.contains("(transport error)") {
                            return Ok(BlockedByFetch::Unreachable(message));
                        }
                        return Ok(BlockedByFetch::Failed(message));
                    }
                };
                let status = resp.status();
                if status == StatusCode::NOT_MODIFIED {
                    if let Some((body, next_link)) = cache_key.as_deref().and_then(page_cache_hit) {
                        Self::record_success();
                        break (body, next_link);
                    }
                    // The entry is normally there, since If-None-Match is only sent
                    // when it is, but the size cap can clear the map in between.
                    // Dropping the ETag makes the retry unconditional, so the loop
                    // cannot see a second 304
                    cached_etag = None;
                    continue;
                }
                if status == StatusCode::NOT_FOUND || status == StatusCode::FORBIDDEN {
                    let body = resp.text().await.unwrap_or_default();
                    if is_forbidden_rate_limit(status, &body) {
                        Self::maybe_record_rate_limit_from_body(status, &body);
                        return Ok(BlockedByFetch::Failed(body));
                    }
                    return Ok(BlockedByFetch::Unavailable);
                }
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    Self::maybe_record_rate_limit_from_body(status, &body);
                    return Ok(BlockedByFetch::Failed(format!("{status}: {body}")));
                }
                Self::record_success();
                let page_next = parse_link_next(resp.headers());
                let etag = resp
                    .headers()
                    .get(header::ETAG)
                    .and_then(|v| v.to_str().ok())
                    .map(ToString::to_string);
                let body = match resp.text().await {
                    Ok(body) => body,
                    Err(err) => {
                        return Ok(BlockedByFetch::Failed(format!(
                            "blocked_by body read failed: {err}"
                        )));
                    }
                };
                // Cache only a body that parses, or a 304 would replay it forever
                if serde_json::from_str::<Vec<BlockedByPayload>>(&body).is_ok()
                    && let (Some(key), Some(etag)) = (cache_key.clone(), etag)
                {
                    page_cache_store(key, etag, body.clone(), page_next.clone());
                }
                break (body, page_next);
            };
            let page: Vec<BlockedByPayload> = match serde_json::from_str(&body) {
                Ok(page) => page,
                Err(err) => {
                    return Ok(BlockedByFetch::Failed(format!(
                        "blocked_by parse failed: {err}"
                    )));
                }
            };
            for item in page {
                if item.number <= 0 {
                    continue;
                }
                let item_repo = item
                    .repository
                    .as_ref()
                    .map(|r| r.full_name.clone())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| repo.to_string());
                found.push(BlockedByIssue {
                    github_id: item.id,
                    repo: item_repo,
                    number: item.number,
                    state: item.state,
                    state_reason: item.state_reason,
                });
            }
            next = page_next;
        }
        Ok(BlockedByFetch::Native(found))
    }

    /// `POST /repos/{owner}/{repo}/issues/{issue_number}/dependencies/blocked_by`
    /// with `{"issue_id": blocking_issue_id}`. Same API version as `list_blocked_by`.
    /// 201 is success. A body that says the link already exists is also success
    pub async fn add_blocked_by(
        &self,
        repo: &str,
        issue_number: u64,
        blocking_issue_id: i64,
    ) -> anyhow::Result<()> {
        let url =
            format!("{GITHUB_API}/repos/{repo}/issues/{issue_number}/dependencies/blocked_by");
        let resp = self
            .dependency_request(
                &url,
                reqwest::Method::POST,
                Some(serde_json::json!({ "issue_id": blocking_issue_id })),
                None,
            )
            .await?;
        let status = resp.status();
        if status.is_success() {
            Self::record_success();
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        if body.to_ascii_lowercase().contains("already") {
            return Ok(());
        }
        Self::maybe_record_rate_limit_from_body(status, &body);
        anyhow::bail!("add blocked_by failed ({status}): {body}")
    }

    /// Remove a blocked-by link. `issue_id` in the path is the blocking issue's database id
    pub async fn remove_blocked_by(
        &self,
        repo: &str,
        number: u64,
        blocking_issue_id: u64,
    ) -> anyhow::Result<()> {
        let url = format!(
            "{GITHUB_API}/repos/{repo}/issues/{number}/dependencies/blocked_by/{blocking_issue_id}"
        );
        self.delete(&url).await?;
        Ok(())
    }

    /// GET or POST an issue-dependency URL with API version 2026-03-10
    async fn dependency_request(
        &self,
        url: &str,
        method: reqwest::Method,
        body: Option<serde_json::Value>,
        if_none_match: Option<&str>,
    ) -> anyhow::Result<Response> {
        Self::check_backoff(url)?;
        let auth = self.auth_header(url).await?;
        let make_req = || {
            let mut req = self
                .client
                .request(method.clone(), url)
                .header(header::AUTHORIZATION, auth.clone())
                .header(header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2026-03-10");
            if method == reqwest::Method::GET {
                req = req.timeout(READ_TIMEOUT);
            }
            if let Some(etag) = if_none_match {
                req = req.header(header::IF_NONE_MATCH, etag);
            }
            if let Some(ref payload) = body {
                req = req.json(payload);
            }
            Ok(req)
        };
        self.send_with_retries(make_req, false).await
    }

    /// Get a single issue.
    pub async fn get_issue(&self, repo: &str, number: &str) -> anyhow::Result<GitHubIssue> {
        self.get_json(&format!("{}/repos/{repo}/issues/{number}", self.api_root()))
            .await
    }

    /// Check whether an issue number refers to a pull request.
    pub async fn is_pull_request(&self, repo: &str, number: &str) -> bool {
        self.get_issue(repo, number)
            .await
            .is_ok_and(|i| i.pull_request.is_some())
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
        // An empty label lists every issue
        let mut query = vec![("state", state), ("per_page", "100")];
        if !label.is_empty() {
            query.push(("labels", label));
        }
        let all: Vec<GitHubIssue> = self.get_all_pages(&url, &query).await?;
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
    ///
    /// The URL carries no `since`, so it repeats across calls and `get_all_pages` can send
    /// it conditionally. GitHub answers 304 while nothing changed, which is free quota
    pub async fn list_all_open_issues(&self, repo: &str) -> anyhow::Result<Vec<GitHubIssue>> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues");
        let params = [("state", "open"), ("per_page", "100")];
        let all: Vec<GitHubIssue> = self.get_all_pages(&url, &params).await?;
        // GitHub /issues API returns PRs too — filter them out
        Ok(all
            .into_iter()
            .filter(|i| i.pull_request.is_none())
            .collect())
    }

    /// List open pull requests (paginated) using the REST pulls endpoint.
    /// Returns raw JSON values for flexibility in callers.
    pub async fn list_open_pulls(&self, repo: &str) -> anyhow::Result<Vec<serde_json::Value>> {
        let url = format!("{GITHUB_API}/repos/{repo}/pulls");
        let all: Vec<serde_json::Value> = self
            .get_all_pages(&url, &[("state", "open"), ("per_page", "100")])
            .await?;
        Ok(all)
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
            Err(e) if has_status(&e.to_string(), 404) => {
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
            Err(e) if has_status(&e.to_string(), 404) => {
                tracing::debug!(repo, number, label, "label already removed (404)");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Add a comment to an issue.
    pub async fn add_comment(&self, repo: &str, number: &str, body: &str) -> anyhow::Result<()> {
        let body = redact_posted_body(body);
        let url = format!("{}/repos/{repo}/issues/{number}/comments", self.api_root());
        let payload = serde_json::json!({ "body": body });
        self.post_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Post a commit status. `state` is one of pending, success, failure, error.
    pub async fn post_commit_status(
        &self,
        repo: &str,
        sha: &str,
        state: &str,
        context: &str,
        description: &str,
    ) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/statuses/{sha}");
        let payload = serde_json::json!({
            "state": state,
            "context": context,
            "description": description,
        });
        self.post_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Add a reaction to an issue.
    ///
    /// Supported content values: "+1", "-1", "laugh", "confused", "heart", "hooray", "rocket", "eyes"
    pub async fn add_reaction(
        &self,
        repo: &str,
        number: &str,
        content: &str,
    ) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues/{number}/reactions");
        let payload = serde_json::json!({ "content": content });
        self.post_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Add a reaction to an issue comment.
    ///
    /// Supported content values: "+1", "-1", "laugh", "confused", "heart", "hooray", "rocket", "eyes"
    pub async fn add_comment_reaction(
        &self,
        repo: &str,
        comment_id: &str,
        content: &str,
    ) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/issues/comments/{comment_id}/reactions");
        let payload = serde_json::json!({ "content": content });
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

    /// Whether a merged PR exists whose head ref is `branch`.
    ///
    /// The closed-PR list can include a PR for a different branch, so this
    /// uses [`Self::get_closed_pr_state`], which skips a mismatched `head.ref`
    pub async fn is_pr_merged(&self, repo: &str, branch: &str) -> anyhow::Result<bool> {
        Ok(self
            .get_closed_pr_state(repo, branch)
            .await?
            .is_some_and(|(_, merged, _)| merged))
    }

    /// Get the state of a closed PR for the given branch.
    ///
    /// Returns `Some((merged, state))` if a closed PR exists for the branch,
    /// where `merged` indicates if it was merged and `state` is the PR state ("closed" or "merged").
    /// Returns `None` if no closed PR exists for the branch.
    /// Get the state of a closed PR for a given branch.
    ///
    /// Returns `Some((pr_number, merged, state))` if a closed PR is found that
    /// matches the expected branch. Returns `None` if no matching closed PR is found.
    ///
    /// Validates that the returned PR's head ref matches the expected branch to
    /// prevent incorrect merge decisions when multiple PRs exist for the same branch.
    pub async fn get_closed_pr_state(
        &self,
        repo: &str,
        branch: &str,
    ) -> anyhow::Result<Option<(u64, bool, String)>> {
        let owner = repo
            .split('/')
            .next()
            .ok_or_else(|| anyhow::anyhow!("invalid repo format: {repo}"))?;
        let head = format!("{owner}:{branch}");

        let url = format!("{}/repos/{repo}/pulls", self.api_root());
        let prs: Vec<serde_json::Value> = self
            .get_with_query(
                &url,
                &[("head", &head), ("state", "closed"), ("per_page", "5")],
            )
            .await?;

        if prs.is_empty() {
            return Ok(None);
        }

        // Find the first PR where head ref actually matches our branch
        // This validates that we're working with the correct PR
        for pr in prs {
            let pr_branch = pr
                .get("head")
                .and_then(|h| h.get("ref"))
                .and_then(|r| r.as_str())
                .unwrap_or("");

            if pr_branch != branch {
                tracing::debug!(
                    repo,
                    branch,
                    pr_branch,
                    "skipping PR with mismatched head ref"
                );
                continue;
            }

            let pr_number = pr
                .get("number")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| anyhow::anyhow!("PR missing number field"))?;

            let merged_at = pr.get("merged_at");
            let merged = merged_at.is_some_and(|v| !v.is_null());
            let state = pr
                .get("state")
                .and_then(|v| v.as_str())
                .unwrap_or("closed")
                .to_string();

            return Ok(Some((pr_number, merged, state)));
        }

        // No PR with matching head ref found
        Ok(None)
    }

    /// Batch-check whether the PR for each branch was merged via a single GraphQL query.
    ///
    /// Returns a map of branch → merged. Branches with no matching PR are omitted
    /// (callers should treat missing entries as not merged).
    pub async fn batch_is_pr_merged_by_branch(
        &self,
        repo: &str,
        branches: &[String],
    ) -> anyhow::Result<HashMap<String, bool>> {
        if branches.is_empty() {
            return Ok(HashMap::new());
        }

        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("invalid repo format: {repo}"))?;

        // Build one alias per branch using positional index to avoid GraphQL
        // alias restrictions on branch names that contain non-identifier chars.
        // Branch names are passed via variables ($b0, $b1, …) to avoid injection.
        let mut var_defs = String::new();
        let mut aliases = String::new();
        let mut vars = serde_json::json!({ "owner": owner, "name": name });
        for (i, branch) in branches.iter().enumerate() {
            let _ = write!(var_defs, ", $b{i}: String!");
            let _ = write!(
                aliases,
                r"b{i}: pullRequests(headRefName: $b{i}, states: [MERGED], last: 1) {{ nodes {{ merged }} }} "
            );
            vars[format!("b{i}")] = serde_json::Value::String(branch.clone());
        }

        let query = format!(
            r"query($owner: String!, $name: String!{var_defs}) {{
                repository(owner: $owner, name: $name) {{ {aliases} }}
            }}"
        );

        let resp = self.graphql_with_vars(&query, vars).await?;

        // Surface GraphQL partial errors before checking data structure
        if let Some(errors) = resp.get("errors").and_then(|e| e.as_array())
            && !errors.is_empty()
        {
            let messages: Vec<&str> = errors
                .iter()
                .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                .collect();
            tracing::warn!(
                repo,
                errors = ?messages,
                "GraphQL returned errors in batch PR merge check"
            );
        }

        Self::parse_batch_pr_merged_response(&resp, branches)
    }

    fn parse_batch_pr_merged_response(
        resp: &serde_json::Value,
        branches: &[String],
    ) -> anyhow::Result<HashMap<String, bool>> {
        let repo_data = resp.pointer("/data/repository").ok_or_else(|| {
            // Check for errors array to provide a more helpful error message
            let error_msg = resp
                .get("errors")
                .and_then(|e| e.as_array())
                .and_then(|arr| arr.first())
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("no error details");
            anyhow::anyhow!("missing /data/repository in GraphQL response: {error_msg}")
        })?;

        let mut result = HashMap::new();
        for (i, branch) in branches.iter().enumerate() {
            let alias = format!("b{i}");
            let merged = repo_data
                .get(&alias)
                .and_then(|v| v.get("nodes"))
                .and_then(|nodes| nodes.as_array())
                .and_then(|arr| arr.first())
                .and_then(|node| node.get("merged"))
                .and_then(serde_json::Value::as_bool)
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
        let url = format!("{}/repos/{repo}/issues/comments", self.api_root());
        self.get_all_pages(&url, &[("since", since), ("per_page", "100")])
            .await
    }

    /// Get all comments for a specific issue number.
    pub async fn get_issue_comments(
        &self,
        repo: &str,
        issue_number: u64,
    ) -> anyhow::Result<Vec<GitHubComment>> {
        let url = format!(
            "{}/repos/{repo}/issues/{issue_number}/comments",
            self.api_root()
        );
        self.get_all_pages(&url, &[("per_page", "100")]).await
    }

    /// Get the current authenticated username (cached after first call).
    pub async fn get_whoami(&self) -> anyhow::Result<String> {
        if let Some(cached) = self.cached_username.get() {
            return Ok(cached.clone());
        }

        // An installation token cannot read `GET /user`, and the owner is the identity mentions name
        let mut owner = self.clone();
        owner.owner_only = true;
        let user: serde_json::Value = owner.get_json(&format!("{GITHUB_API}/user")).await?;
        let username = user
            .get("login")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("failed to get current user"))?;

        let _ = self.cached_username.set(username.clone());
        Ok(username)
    }

    /// Get PR number by branch name.
    pub async fn get_pr_number(&self, repo: &str, branch: &str) -> anyhow::Result<Option<u64>> {
        let owner = repo
            .split('/')
            .next()
            .ok_or_else(|| anyhow::anyhow!("invalid repo format: {repo}"))?;
        let head = format!("{owner}:{branch}");

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
            .and_then(serde_json::Value::as_u64)
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
        #[derive(Serialize)]
        struct CreatePullRequest<'a> {
            title: &'a str,
            body: &'a str,
            head: &'a str,
            base: &'a str,
        }

        let body = redact_posted_body(body);
        let url = format!("{}/repos/{repo}/pulls", self.api_root());
        Self::check_backoff(&url)?;

        let payload = CreatePullRequest {
            title,
            body: &body,
            head,
            base,
        };

        let auth = self.auth_header(&url).await?;
        let make_req = || {
            Ok(self
                .client
                .post(&url)
                .header(header::AUTHORIZATION, auth.clone())
                .header(header::ACCEPT, "application/vnd.github+json")
                .header(header::USER_AGENT, "orch")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .json(&payload))
        };
        let response = self.send_with_retries(make_req, false).await?;
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
            .ok_or_else(|| anyhow::anyhow!("failed to get PR URL from response: {pr:?}"))
    }

    /// Get the repository's default branch from the GitHub API.
    ///
    /// Used as a fallback when local git detection yields an invalid base
    /// branch for PR creation.
    pub async fn get_default_branch(&self, repo: &str) -> anyhow::Result<String> {
        let url = format!("{}/repos/{repo}", self.api_root());
        let resp: serde_json::Value = self.get_json(&url).await?;
        resp.get("default_branch")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("default_branch missing from repo response"))
    }

    /// Get PR details by PR number.
    /// Returns the full PR object including the `mergeable` field.
    pub async fn get_pr(&self, repo: &str, pr_number: u64) -> anyhow::Result<GitHubPullRequest> {
        let url = format!("{}/repos/{repo}/pulls/{pr_number}", self.api_root());
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
    ) -> anyhow::Result<HashMap<u64, (bool, String)>> {
        // GitHub GraphQL complexity limit: chunk at 50 to avoid slow/rejected queries.
        const CHUNK_SIZE: usize = 50;

        if pr_numbers.is_empty() {
            return Ok(HashMap::new());
        }

        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("invalid repo format: {repo}"))?;

        let mut result = HashMap::new();

        for chunk in pr_numbers.chunks(CHUNK_SIZE) {
            let aliases: String = chunk
                .iter()
                .map(|n| format!("pr{n}: pullRequest(number: {n}) {{ merged state }}"))
                .collect::<Vec<_>>()
                .join("\n  ");

            let query = format!(
                r"query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ {aliases} }} }}"
            );

            let resp = self
                .graphql_with_vars(&query, serde_json::json!({ "owner": owner, "name": name }))
                .await?;
            let repo_data = resp
                .pointer("/data/repository")
                .ok_or_else(|| anyhow::anyhow!("missing /data/repository in GraphQL response"))?;

            for n in chunk {
                if let Some(pr) = repo_data.get(format!("pr{n}")) {
                    let merged = pr
                        .get("merged")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let state = pr
                        .get("state")
                        .and_then(|v| v.as_str())
                        .unwrap_or("UNKNOWN")
                        .to_lowercase();
                    result.insert(*n, (merged, state));
                }
            }
        }

        Ok(result)
    }

    /// Batch-fetch state and labels for multiple GitHub issues in a single GraphQL request.
    ///
    /// Returns a map of issue number (as string) → `(state, labels)` where state is
    /// lowercase ("open" / "closed") to match the REST API convention used elsewhere.
    pub async fn batch_get_issue_states(
        &self,
        repo: &str,
        issue_numbers: &[u64],
        caller_context: &str,
    ) -> anyhow::Result<HashMap<String, (String, Vec<String>)>> {
        // GitHub GraphQL complexity limit: chunk at 50 to avoid slow/rejected queries.
        const CHUNK_SIZE: usize = 50;

        if issue_numbers.is_empty() {
            return Ok(HashMap::new());
        }

        let (valid_issue_numbers, invalid_issue_numbers) =
            sanitize_github_issue_numbers(issue_numbers);
        if !invalid_issue_numbers.is_empty() {
            tracing::warn!(
                repo,
                caller_context,
                invalid_issue_numbers = ?invalid_issue_numbers,
                total_supplied = issue_numbers.len(),
                "dropping invalid GitHub issue numbers before batch issue-state GraphQL query"
            );
        }
        if valid_issue_numbers.is_empty() {
            return Ok(HashMap::new());
        }

        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("invalid repo format: {repo}"))?;

        let mut result = HashMap::new();

        for chunk in valid_issue_numbers.chunks(CHUNK_SIZE) {
            let aliases: String = chunk
                .iter()
                .map(|n| {
                    format!("issue{n}: issue(number: {n}) {{ state labels(first: 100) {{ nodes {{ name }} }} }}")
                })
                .collect::<Vec<_>>()
                .join(" ");

            let query = format!(
                r"query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ {aliases} }} }}"
            );

            let resp = self
                .graphql_with_vars(&query, serde_json::json!({ "owner": owner, "name": name }))
                .await?;
            let repo_data = resp
                .pointer("/data/repository")
                .ok_or_else(|| anyhow::anyhow!("missing /data/repository in GraphQL response"))?;

            for n in chunk {
                if let Some(issue) = repo_data.get(format!("issue{n}")) {
                    let state = issue
                        .get("state")
                        .and_then(|v| v.as_str())
                        .unwrap_or("UNKNOWN")
                        .to_lowercase();
                    let labels: Vec<String> = issue
                        .pointer("/labels/nodes")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|l| {
                                    l.get("name").and_then(|nm| nm.as_str()).map(String::from)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    result.insert(n.to_string(), (state, labels));
                }
            }
        }
        Ok(result)
    }

    /// Required status checks for a branch from classic protection and from rulesets
    pub async fn get_required_status_check_contexts(
        &self,
        repo: &str,
        branch: &str,
    ) -> anyhow::Result<Vec<RequiredStatusCheck>> {
        let branch = urlencoding::encode(branch);
        let classic_url = format!(
            "{}/repos/{repo}/branches/{branch}/protection",
            self.api_root()
        );
        let rules_url = format!("{}/repos/{repo}/rules/branches/{branch}", self.api_root());

        let classic_fut = self.get_json::<serde_json::Value>(&classic_url);
        let rules_fut = self.get_all_pages::<serde_json::Value>(&rules_url, &[("per_page", "100")]);
        let (classic_resp, rules_resp) = tokio::join!(classic_fut, rules_fut);

        let classic = match classic_resp {
            Ok(value) => parse_classic_required_status_checks(&value),
            Err(e) => {
                if is_no_branch_protection_error(&e.to_string()) {
                    Vec::new()
                } else {
                    return Err(e);
                }
            }
        };

        let ruleset = match rules_resp {
            Ok(rules) => parse_ruleset_required_status_checks(&rules),
            Err(e) => {
                if has_status(&e.to_string(), 404) {
                    Vec::new()
                } else {
                    return Err(e);
                }
            }
        };

        Ok(merge_required_status_checks(classic, ruleset))
    }

    /// Get commit status contexts for a SHA.
    pub async fn get_commit_status_contexts(
        &self,
        repo: &str,
        sha: &str,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let url = format!(
            "{}/repos/{repo}/commits/{sha}/status?per_page=100",
            self.api_root()
        );
        // The /commits/{sha}/status endpoint returns a wrapper object:
        // { "state": "pending", "total_count": N, "statuses": [...] }
        // Use get_all_pages_from_wrapper to follow pagination and extract the
        // `statuses` array across pages so we don't silently truncate at 100.
        let statuses: Vec<serde_json::Value> =
            self.get_all_pages_from_wrapper(&url, "statuses").await?;

        Ok(statuses
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
        mergeable_state: Option<&str>,
    ) -> String {
        if failing > 0 {
            "failure".to_string()
        } else if pending > 0 {
            "pending".to_string()
        } else if total == 0 && has_workflows {
            // No check runs yet. If GitHub already reports mergeable_state="clean"
            // the PR is ready (e.g. all workflows use paths-ignore and skipped this
            // commit). Treating it as pending would stall auto-merge forever.
            if mergeable_state == Some("clean") {
                "success".to_string()
            } else {
                "pending".to_string()
            }
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
                .is_some_and(|workflows| !workflows.is_empty())),
            Err(e) => {
                let err = e.to_string();
                if has_status(&err, 404) {
                    Ok(false)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// True when `.github/workflows` on `branch` has at least one YAML file
    pub async fn has_workflow_on_branch(&self, repo: &str, branch: &str) -> anyhow::Result<bool> {
        let branch = urlencoding::encode(branch);
        let url = format!(
            "{}/repos/{repo}/contents/.github/workflows?ref={branch}",
            self.api_root()
        );
        match self.get_json::<serde_json::Value>(&url).await {
            Ok(value) => Ok(value.as_array().is_some_and(|entries| {
                entries.iter().any(|entry| {
                    entry
                        .get("name")
                        .and_then(|name| name.as_str())
                        .is_some_and(|name| {
                            Path::new(name)
                                .extension()
                                .and_then(|ext| ext.to_str())
                                .is_some_and(|ext| {
                                    ext.eq_ignore_ascii_case("yml")
                                        || ext.eq_ignore_ascii_case("yaml")
                                })
                        })
                })
            })),
            Err(e) => {
                let err = e.to_string();
                if has_status(&err, 404) {
                    Ok(false)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// True when `path` exists on `branch`
    pub async fn has_file_on_branch(
        &self,
        repo: &str,
        path: &str,
        branch: &str,
    ) -> anyhow::Result<bool> {
        let path = path
            .split('/')
            .map(|segment| urlencoding::encode(segment).into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let branch = urlencoding::encode(branch);
        let url = format!(
            "{}/repos/{repo}/contents/{path}?ref={branch}",
            self.api_root()
        );
        match self.get_json::<serde_json::Value>(&url).await {
            Ok(_) => Ok(true),
            Err(e) => {
                let err = e.to_string();
                if has_status(&err, 404) {
                    Ok(false)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// SHA of the head commit on `branch`
    pub async fn get_branch_sha(&self, repo: &str, branch: &str) -> anyhow::Result<String> {
        let encoded = urlencoding::encode(branch);
        let url = format!("{}/repos/{repo}/branches/{encoded}", self.api_root());
        let resp: serde_json::Value = self.get_json(&url).await?;
        resp.pointer("/commit/sha")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("branch {branch} has no commit sha"))
    }

    /// Create a repository ruleset that requires `checks` on the default branch
    pub async fn create_required_check_ruleset(
        &self,
        repo: &str,
        checks: &[RequiredStatusCheck],
    ) -> anyhow::Result<()> {
        let url = format!("{}/repos/{repo}/rulesets", self.api_root());
        let payload = required_check_ruleset_payload(checks);
        self.post_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Id and required checks of the repository ruleset named `name`, if it exists
    pub async fn get_named_ruleset_required_checks(
        &self,
        repo: &str,
        name: &str,
    ) -> anyhow::Result<Option<(i64, Vec<RequiredStatusCheck>)>> {
        let url = format!("{}/repos/{repo}/rulesets", self.api_root());
        let rulesets: Vec<serde_json::Value> =
            self.get_all_pages(&url, &[("per_page", "100")]).await?;
        let Some(id) = rulesets.iter().find_map(|ruleset| {
            if ruleset.get("name").and_then(|n| n.as_str()) != Some(name) {
                return None;
            }
            ruleset.get("id").and_then(serde_json::Value::as_i64)
        }) else {
            return Ok(None);
        };
        let url = format!("{}/repos/{repo}/rulesets/{id}", self.api_root());
        let value: serde_json::Value = self.get_json(&url).await?;
        let rules = value
            .get("rules")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(Some((id, parse_ruleset_required_status_checks(&rules))))
    }

    /// Replace the required checks on an existing repository ruleset
    pub async fn update_required_check_ruleset(
        &self,
        repo: &str,
        ruleset_id: i64,
        checks: &[RequiredStatusCheck],
    ) -> anyhow::Result<()> {
        let url = format!("{}/repos/{repo}/rulesets/{ruleset_id}", self.api_root());
        let payload = required_check_ruleset_payload(checks);
        self.put_json_raw(&url, &payload).await?;
        Ok(())
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
        //
        // Use `per_page=100` and manual pagination via `get_all_pages_from_wrapper`
        // because this endpoint returns a *wrapper object* — `{ "total_count": N,
        // "check_runs": [...] }` — rather than a raw JSON array.  `get_all_pages`
        // cannot be used here because it calls `serde_json::from_str::<Vec<T>>`
        // which would fail on an object body.
        let url = format!(
            "{}/repos/{repo}/commits/{sha}/check-runs?filter=latest&per_page=100",
            self.api_root()
        );
        self.get_all_pages_from_wrapper(&url, "check_runs").await
    }

    /// Get annotations for a specific check run.
    ///
    /// Returns the list of annotations attached to the check run, including
    /// billing failure messages ("The job was not started because recent account
    /// payments have failed …") that GitHub surfaces as annotations rather than
    /// as a distinct CI conclusion value.
    pub async fn get_check_run_annotations(
        &self,
        repo: &str,
        check_run_id: u64,
    ) -> anyhow::Result<Vec<GitHubAnnotation>> {
        let url = format!("{GITHUB_API}/repos/{repo}/check-runs/{check_run_id}/annotations");
        self.get_all_pages(&url, &[("per_page", "100")]).await
    }

    /// Last `lines` lines of the log of an Actions job. A check run id is the job id
    pub async fn get_job_log_tail(
        &self,
        repo: &str,
        job_id: u64,
        lines: usize,
    ) -> anyhow::Result<String> {
        let url = format!("{GITHUB_API}/repos/{repo}/actions/jobs/{job_id}/logs");
        let bytes = self.get_bytes(&url).await?;
        let text = String::from_utf8_lossy(&bytes);
        let all: Vec<&str> = text.lines().collect();
        let start = all.len().saturating_sub(lines);
        Ok(all[start..].join("\n"))
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
            .map(ToString::to_string)
            .ok_or_else(|| anyhow::anyhow!("commit {sha} has no committer date"))
    }

    /// Update the body of a pull request.
    pub async fn update_pr_body(
        &self,
        repo: &str,
        pr_number: u64,
        body: &str,
    ) -> anyhow::Result<()> {
        let body = redact_posted_body(body);
        let url = format!("{}/repos/{repo}/pulls/{pr_number}", self.api_root());
        let payload = serde_json::json!({ "body": body });
        self.patch_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Close a pull request without merging it.
    pub async fn close_pr(&self, repo: &str, pr_number: u64) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/pulls/{pr_number}");
        let payload = serde_json::json!({ "state": "closed" });
        self.patch_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// How many commits the head is missing from `base` (GitHub's `behind_by`).
    ///
    /// Zero means the head already contains the current base tip, so its checks
    /// ran against the code the merge would produce.
    pub async fn compare_behind_by(
        &self,
        repo: &str,
        base: &str,
        head_sha: &str,
    ) -> anyhow::Result<u64> {
        let url = format!("{GITHUB_API}/repos/{repo}/compare/{base}...{head_sha}");
        let body: serde_json::Value = self.get_json(&url).await?;
        parse_behind_by(&body)
    }

    /// Parents, author and committer of a commit
    pub async fn commit_meta(&self, repo: &str, sha: &str) -> anyhow::Result<CommitMeta> {
        let url = format!("{GITHUB_API}/repos/{repo}/commits/{sha}");
        let value: serde_json::Value = self.get_json(&url).await?;
        Ok(parse_commit_meta(&value))
    }

    /// True when `branch` already contains the commit `sha`
    pub async fn branch_contains(
        &self,
        repo: &str,
        branch: &str,
        sha: &str,
    ) -> anyhow::Result<bool> {
        let url = format!("{GITHUB_API}/repos/{repo}/compare/{branch}...{sha}");
        let body: serde_json::Value = self.get_json(&url).await?;
        body.get("ahead_by")
            .and_then(serde_json::Value::as_u64)
            .map(|ahead| ahead == 0)
            .ok_or_else(|| anyhow::anyhow!("compare response has no numeric ahead_by field"))
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
        let url = format!("{}/repos/{repo}/issues/{number}", self.api_root());
        let payload = serde_json::json!({ "state": "open" });
        self.patch_json_raw(&url, &payload).await?;
        Ok(())
    }

    /// Check if a user is a collaborator.
    pub async fn is_collaborator(&self, repo: &str, username: &str) -> anyhow::Result<bool> {
        let url = format!("{GITHUB_API}/repos/{repo}/collaborators/{username}");
        Self::check_backoff(&url)?;
        let auth = self.auth_header(&url).await?;
        let make_req = || {
            Ok(self
                .client
                .get(&url)
                .timeout(READ_TIMEOUT)
                .header(header::AUTHORIZATION, auth.clone())
                .header(header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"))
        };
        let resp = self.send_with_retries(make_req, false).await?;
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
                addSubIssue(input: {{issueId: "{parent_node_id}", subIssueId: "{child_node_id}"}}) {{
                    issue {{ number }}
                    subIssue {{ number }}
                }}
            }}"#
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
            anyhow::bail!("invalid repo format: expected 'owner/repo', got '{repo}'");
        }
        let (owner, repo_name) = (parts[0], parts[1]);

        // First, get the repository and issue node IDs
        let query = r"query($owner: String!, $name: String!, $number: Int!) {
                repository(owner: $owner, name: $name) {
                    id
                    issue(number: $number) {
                        id
                    }
                }
            }";
        let vars = serde_json::json!({
            "owner": owner,
            "name": repo_name,
            "number": issue_number,
        });

        let result = self.graphql_with_vars(query, vars).await?;

        let repo_id = result
            .pointer("/data/repository/id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("failed to get repository node ID"))?;

        let issue_id = result
            .pointer("/data/repository/issue/id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("failed to get issue node ID"))?;

        // Fetch branch ref to get the commit OID required by createLinkedBranch
        let branch_query = r"query($owner: String!, $name: String!, $ref: String!) {
                repository(owner: $owner, name: $name) {
                    ref(qualifiedName: $ref) {
                        target {
                            oid
                        }
                    }
                }
            }";
        let branch_vars = serde_json::json!({
            "owner": owner,
            "name": repo_name,
            "ref": format!("refs/heads/{}", branch),
        });

        let branch_result = self.graphql_with_vars(branch_query, branch_vars).await?;
        let branch_oid = branch_result
            .pointer("/data/repository/ref/target/oid")
            .and_then(|v| v.as_str());

        // If branch doesn't exist yet, use the default branch OID.
        // createLinkedBranch will create the branch at that OID AND link it.
        let branch_oid = if let Some(oid) = branch_oid {
            oid.to_string()
        } else {
            // Get default branch OID as the base for the new branch
            let default_query = r"query($owner: String!, $name: String!) {
                    repository(owner: $owner, name: $name) {
                        defaultBranchRef {
                            target {
                                oid
                            }
                        }
                    }
                }";
            let default_vars = serde_json::json!({
                "owner": owner,
                "name": repo_name,
            });
            let default_result = self.graphql_with_vars(default_query, default_vars).await?;
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
                    return Ok(format!("unlinked:{branch}"));
                }
            }
        };

        // Use createLinkedBranch mutation to link the issue to the existing branch.
        // Required fields: issueId, repositoryId, name (branch name), oid (commit SHA).
        let mutation = r"mutation($issueId: ID!, $repositoryId: ID!, $name: String!, $oid: GitObjectID!) {
                createLinkedBranch(input: {
                    issueId: $issueId
                    repositoryId: $repositoryId
                    name: $name
                    oid: $oid
                }) {
                    linkedBranch {
                        id
                        ref {
                            name
                        }
                    }
                }
            }";
        let mutation_vars = serde_json::json!({
            "issueId": issue_id,
            "repositoryId": repo_id,
            "name": branch,
            "oid": branch_oid,
        });

        let link_result = self.graphql_with_vars(mutation, mutation_vars).await;

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
                            .is_some_and(|m| m.contains("already"))
                    }) {
                        tracing::debug!(
                            repo,
                            issue_number,
                            branch,
                            "issue already linked to branch"
                        );
                        Ok(format!("already_linked:{branch}"))
                    } else {
                        anyhow::bail!("failed to link issue to branch: {result:?}")
                    }
                }
            }
            Err(e) => {
                // Check if it's a "already exists" type error
                let err_str = format!("{e}");
                if err_str.contains("already") || err_str.contains("existing") {
                    tracing::debug!(
                        repo,
                        issue_number,
                        branch,
                        "issue already linked to branch (error check)"
                    );
                    Ok(format!("already_linked:{branch}"))
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Merge a PR using the REST API (squash merge).
    /// GitHub rejects it when the PR head is no longer `head_sha`, the commit CI was checked against
    pub async fn merge_pr(
        &self,
        repo: &str,
        pr_number: u64,
        delete_branch: bool,
        head_sha: &str,
    ) -> anyhow::Result<()> {
        if !is_full_commit_sha(head_sha) {
            anyhow::bail!("merge_pr requires the full head sha, got {head_sha:?}");
        }
        let url = format!("{GITHUB_API}/repos/{repo}/pulls/{pr_number}/merge");
        Self::check_backoff(&url)?;
        let payload = merge_pr_body(head_sha);
        let auth = self.auth_header(&url).await?;
        let make_req = || {
            Ok(self
                .client
                .put(&url)
                .json(&payload)
                .header(header::AUTHORIZATION, auth.clone())
                .header(header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"))
        };
        let resp = self.send_with_retries(make_req, false).await?;
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
                    let err_str = e.to_string();
                    if err_str.contains("Reference does not exist") {
                        tracing::debug!(
                            ref_name,
                            "branch already gone (GitHub auto-deleted on merge)"
                        );
                    } else {
                        tracing::warn!(ref_name, err = %e, "failed to delete branch after merge");
                    }
                }
            }
        }
        Ok(())
    }

    /// Update a PR's branch by merging the latest base branch into it
    /// (equivalent of the "Update branch" button on GitHub, or `gh pr
    /// update-branch`). Produces a new merge commit on the head branch,
    /// which re-triggers CI — used to give a PR a chance to pick up a fix
    /// that landed on the base branch after the PR's last CI run failed.
    pub async fn update_pr_branch(&self, repo: &str, pr_number: u64) -> anyhow::Result<()> {
        let url = format!("{GITHUB_API}/repos/{repo}/pulls/{pr_number}/update-branch");
        Self::check_backoff(&url)?;
        let auth = self.auth_header(&url).await?;
        let make_req = || {
            Ok(self
                .client
                .put(&url)
                .header(header::AUTHORIZATION, auth.clone())
                .header(header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"))
        };
        let resp = self.send_with_retries(make_req, false).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            Self::maybe_record_rate_limit_from_body(status, &body);
            anyhow::bail!("GitHub PR update-branch failed ({status}): {body}");
        }
        Self::record_success();
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
        let pr_query = r"query($owner: String!, $name: String!, $number: Int!) {
            repository(owner: $owner, name: $name) {
                pullRequest(number: $number) { id }
            }
        }";
        let body = self
            .graphql_with_vars(
                pr_query,
                serde_json::json!({
                    "owner": owner,
                    "name": repo_name,
                    "number": pr_number,
                }),
            )
            .await?;
        let pr_id = body
            .pointer("/data/repository/pullRequest/id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("could not get PR node ID"))?
            .to_string();

        // Enable auto-merge with squash
        let mutation = r"mutation($prId: ID!) {
            enablePullRequestAutoMerge(input: {pullRequestId: $prId, mergeMethod: SQUASH}) {
                pullRequest { autoMergeRequest { enabledAt } }
            }
        }";
        let body = self
            .graphql_with_vars(mutation, serde_json::json!({ "prId": pr_id }))
            .await?;
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
            anyhow::bail!("invalid repo format: expected 'owner/repo', got '{repo}'");
        }
        let (owner, repo_name) = (parts[0], parts[1]);
        let number = match parse_positive_issue_number(number) {
            Ok(number) => number,
            Err(e) => {
                tracing::debug!(repo, issue = number, ?e, "skipping sub-issues lookup");
                return Ok(Vec::new());
            }
        };

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

            let (query, vars) = if let Some(ref c) = cursor {
                (
                    r"query($owner: String!, $name: String!, $number: Int!, $first: Int!, $after: String!) {
                    repository(owner: $owner, name: $name) {
                        issue(number: $number) {
                            subIssues(first: $first, after: $after) {
                                nodes { number }
                                pageInfo { hasNextPage endCursor }
                            }
                        }
                    }
                }",
                    serde_json::json!({
                        "owner": owner,
                        "name": repo_name,
                        "number": number,
                        "first": page_size,
                        "after": c,
                    }),
                )
            } else {
                (
                    r"query($owner: String!, $name: String!, $number: Int!, $first: Int!) {
                    repository(owner: $owner, name: $name) {
                        issue(number: $number) {
                            subIssues(first: $first) {
                                nodes { number }
                                pageInfo { hasNextPage endCursor }
                            }
                        }
                    }
                }",
                    serde_json::json!({
                        "owner": owner,
                        "name": repo_name,
                        "number": number,
                        "first": page_size,
                    }),
                )
            };

            let result = self
                .graphql_with_headers_and_vars(query, &["GraphQL-Features:sub_issues"], vars)
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
                        .filter_map(|n| n.get("number").and_then(serde_json::Value::as_u64))
                        .collect();
                    all_numbers.extend(numbers);
                }
                None => break,
            }

            let page_info = sub_issues_data.and_then(|s| s.get("pageInfo"));
            let has_next = page_info
                .and_then(|p| p.get("hasNextPage"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);

            if !has_next {
                break;
            }

            cursor = page_info
                .and_then(|p| p.get("endCursor"))
                .and_then(|c| c.as_str())
                .map(ToString::to_string);
        }

        Ok(all_numbers)
    }

    /// Get combined CI check status for a git ref
    ///
    /// Counts come from [`Self::get_check_runs`], which requests `filter=latest`
    /// and follows pagination, so a failure or in-progress run past the first page still counts
    pub async fn get_combined_status(
        &self,
        repo: &str,
        git_ref: &str,
        has_workflows: bool,
        mergeable_state: Option<&str>,
    ) -> anyhow::Result<(String, u64, u64, u64, u64)> {
        let runs = self.get_check_runs(repo, git_ref).await?;
        let (total, passing, failing, pending) = Self::count_ci_runs(&runs);
        let state =
            Self::combined_status_state(total, failing, pending, has_workflows, mergeable_state);

        Ok((state, total, passing, failing, pending))
    }

    /// Count CI runs as (total, passing, failing, pending), skipping the review's own checks
    fn count_ci_runs(runs: &[types::GitHubCheckRun]) -> (u64, u64, u64, u64) {
        let mut total = 0u64;
        let mut passing = 0u64;
        let mut failing = 0u64;
        let mut pending = 0u64;

        for run in runs.iter().filter(|r| !review_gate::is_own_check(&r.name)) {
            total += 1;
            if !run.is_finished() {
                pending += 1;
            } else if run.is_passed() {
                passing += 1;
            } else {
                failing += 1;
            }
        }

        (total, passing, failing, pending)
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
                    && matches!(r["conclusion"].as_str(), Some("failure" | "cancelled"))
            })
            .and_then(|r| r["id"].as_u64())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no failed run found for workflow '{workflow_name}' on branch '{branch}'"
                )
            })?;

        // POST rerun — no body needed
        let rerun_url = format!("{GITHUB_API}/repos/{repo}/actions/runs/{run_id}/rerun");
        let auth = self.auth_header(&rerun_url).await?;
        let make_req = || {
            Ok(self
                .client
                .post(&rerun_url)
                .header(header::AUTHORIZATION, auth.clone())
                .header(header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"))
        };
        let resp = self.send_with_retries(make_req, false).await?;
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
    ) -> anyhow::Result<HashMap<u64, PrReviewBatchData>> {
        if pr_numbers.is_empty() {
            return Ok(HashMap::new());
        }

        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("invalid repo format: {repo}"))?;

        // Build one GraphQL alias per PR number.
        let aliases: String = pr_numbers
            .iter()
            .map(|&n| build_pr_review_alias(n))
            .collect::<Vec<_>>()
            .join("\n");

        let query = format!(
            r"query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ {aliases} }} }}"
        );

        let resp = self
            .graphql_with_vars(&query, serde_json::json!({ "owner": owner, "name": name }))
            .await?;
        let repo_data = resp
            .pointer("/data/repository")
            .ok_or_else(|| anyhow::anyhow!("missing /data/repository in GraphQL response"))?;

        let mut result = HashMap::new();
        for &n in pr_numbers {
            let Some(pr_data) = repo_data.get(format!("pr{n}")) else {
                continue;
            };

            let merged = pr_data
                .get("merged")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let mergeable = match pr_data.get("mergeable").and_then(|v| v.as_str()) {
                Some("MERGEABLE") => Some(true),
                Some("CONFLICTING") => Some(false),
                _ => None, // UNKNOWN or missing
            };

            let mut reviews: Vec<GitHubReview> = pr_data
                .pointer("/reviews/nodes")
                .and_then(|v| v.as_array())
                .map(|nodes| nodes.iter().filter_map(parse_graphql_review).collect())
                .unwrap_or_default();

            if connection_has_next_page(pr_data.pointer("/reviews/pageInfo")) {
                tracing::warn!(
                    pr_number = n,
                    fetched = reviews.len(),
                    "batch review fetch truncated reviews connection at first page"
                );
            }

            // Ensure deterministic ordering by submitted timestamp (oldest -> newest).
            // GraphQL connections do not guarantee a stable sort unless explicitly
            // requested. Sort here so consumers can rely on chronological order.
            reviews.sort_by(|a, b| a.submitted_at.cmp(&b.submitted_at));

            let mut review_comments: Vec<GitHubReviewComment> = pr_data
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

            if connection_has_next_page(pr_data.pointer("/reviewThreads/pageInfo")) {
                tracing::warn!(
                    pr_number = n,
                    fetched_threads = pr_data
                        .pointer("/reviewThreads/nodes")
                        .and_then(|v| v.as_array())
                        .map_or(0, Vec::len),
                    "batch review fetch truncated reviewThreads connection at first page"
                );
            }
            if pr_data
                .pointer("/reviewThreads/nodes")
                .and_then(|v| v.as_array())
                .is_some_and(|threads| {
                    threads.iter().any(|thread| {
                        connection_has_next_page(thread.pointer("/comments/pageInfo"))
                    })
                })
            {
                tracing::warn!(
                    pr_number = n,
                    "batch review fetch truncated one or more review thread comment connections"
                );
            }

            // Sort inline review comments by created_at (oldest -> newest) so
            // callers can process them deterministically.
            review_comments.sort_by(|a, b| a.created_at.cmp(&b.created_at));

            let mut issue_comments: Vec<GitHubComment> = pr_data
                .pointer("/comments/nodes")
                .and_then(|v| v.as_array())
                .map(|nodes| {
                    nodes
                        .iter()
                        .filter_map(parse_graphql_issue_comment)
                        .collect()
                })
                .unwrap_or_default();

            if let Some(cursor) = connection_end_cursor(pr_data.pointer("/comments/pageInfo")) {
                issue_comments.extend(
                    self.fetch_pr_issue_comments_after(owner, name, n, Some(cursor))
                        .await?,
                );
            }

            // Sort issue-level comments by created_at (oldest -> newest). The
            // review poll logic expects chronological ordering and may iterate
            // in reverse to find the latest comments — so ensure a consistent
            // ordering here regardless of GraphQL's default.
            issue_comments.sort_by(|a, b| a.created_at.cmp(&b.created_at));

            result.insert(
                n,
                PrReviewBatchData {
                    merged,
                    head_sha: pr_data
                        .get("headRefOid")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    mergeable,
                    reviews,
                    review_comments,
                    issue_comments,
                },
            );
        }

        Ok(result)
    }

    async fn fetch_pr_issue_comments_after(
        &self,
        owner: &str,
        name: &str,
        pr_number: u64,
        mut cursor: Option<String>,
    ) -> anyhow::Result<Vec<GitHubComment>> {
        let mut comments = Vec::new();
        let mut page_count = 0;
        let max_pages = 50;

        while let Some(current_cursor) = cursor.take() {
            page_count += 1;
            if page_count > max_pages {
                tracing::warn!(
                    pr_number,
                    fetched = comments.len(),
                    "stopping PR issue comment pagination after max page limit"
                );
                break;
            }

            let (query, vars) =
                build_pr_issue_comments_page_query(owner, name, pr_number, &current_cursor);
            let resp = self.graphql_with_vars(&query, vars).await?;
            let comments_data = resp
                .pointer("/data/repository/pullRequest/comments")
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "missing /data/repository/pullRequest/comments in GraphQL response"
                    )
                })?;

            comments.extend(
                comments_data
                    .get("nodes")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(parse_graphql_issue_comment),
            );

            cursor = connection_end_cursor(comments_data.get("pageInfo"));
        }

        Ok(comments)
    }

    /// Check the latest automated review comment on a PR.
    pub async fn get_automated_review_status(
        &self,
        repo: &str,
        pr_number: u64,
    ) -> anyhow::Result<Option<String>> {
        let comments = self.list_comments(repo, &pr_number.to_string()).await?;

        // Find the most recent automated review comment authored by a collaborator or the App
        let app_login = super::app::bot_login().await;
        let mut newest: Option<&types::GitHubComment> = None;
        for c in &comments {
            if !c.body.starts_with("## Automated Review") {
                continue;
            }
            let trusted = match app_login.as_deref() {
                Some(app) if super::app::same_login(&c.user.login, app) => Ok(true),
                _ => self.is_collaborator(repo, &c.user.login).await,
            };
            match trusted {
                Ok(true) => match newest {
                    None => newest = Some(c),
                    Some(prev) => {
                        if c.created_at > prev.created_at {
                            newest = Some(c);
                        }
                    }
                },
                Ok(false) => {
                    tracing::warn!(
                        user = %c.user.login,
                        pr_number,
                        "ignoring automated review comment from non-collaborator"
                    );
                }
                Err(e) => {
                    tracing::debug!(
                        user = %c.user.login,
                        error = %e,
                        "failed to check collaborator status, skipping comment"
                    );
                }
            }
        }

        match newest {
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

/// Read `behind_by` out of a compare response.
///
/// A missing field is an error, not zero: treating an unreadable response as
/// "head is up to date" would let a stale PR merge
/// A commit's parents, first parent first, and the GitHub accounts behind it
#[derive(Debug, Default, PartialEq)]
pub struct CommitMeta {
    pub parents: Vec<String>,
    pub author_login: Option<String>,
    pub committer_login: Option<String>,
}

fn parse_commit_meta(commit: &serde_json::Value) -> CommitMeta {
    let login = |field: &str| {
        commit
            .pointer(&format!("/{field}/login"))
            .and_then(|v| v.as_str())
            .map(String::from)
    };
    CommitMeta {
        parents: commit
            .get("parents")
            .and_then(|p| p.as_array())
            .map(|parents| {
                parents
                    .iter()
                    .filter_map(|p| p.get("sha").and_then(|s| s.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        author_login: login("author"),
        committer_login: login("committer"),
    }
}

fn parse_behind_by(body: &serde_json::Value) -> anyhow::Result<u64> {
    body.get("behind_by")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("compare response has no numeric behind_by field"))
}

/// A branch protection read that means "no protection": 404 "Branch not protected", or the 403
/// True when `text` carries the HTTP status in the `failed (404 ...)` form that every request error
/// here uses, or `HTTP 404` from the CLI. The status code sits apart from the URL, so an issue
/// number such as 1404 does not match
pub fn has_status(text: &str, status: u16) -> bool {
    text.contains(&format!("failed ({status}")) || text.contains(&format!("HTTP {status}"))
}

/// GitHub returns for private repos on the Free plan
fn is_no_branch_protection_error(err: &str) -> bool {
    has_status(err, 404)
        || (err.contains("403")
            && err.contains("Upgrade to GitHub Pro or make this repository public"))
}

pub(crate) const REQUIRED_CHECK_RULESET_NAME: &str = "orch required checks";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RulesetCreateKind {
    CheckNeverRun,
    NameTaken,
    Refused,
    Retryable,
    Other,
}

pub(crate) fn required_check_ruleset_payload(checks: &[RequiredStatusCheck]) -> serde_json::Value {
    let required: Vec<serde_json::Value> = checks
        .iter()
        .map(|check| {
            if let Some(id) = check.integration_id {
                serde_json::json!({ "context": check.context, "integration_id": id })
            } else {
                serde_json::json!({ "context": check.context })
            }
        })
        .collect();
    serde_json::json!({
        "name": REQUIRED_CHECK_RULESET_NAME,
        "target": "branch",
        "enforcement": "active",
        "conditions": {
            "ref_name": {
                "include": ["~DEFAULT_BRANCH"],
                "exclude": []
            }
        },
        "rules": [{
            "type": "required_status_checks",
            "parameters": {
                "strict_required_status_checks_policy": true,
                "do_not_enforce_on_create": false,
                "required_status_checks": required
            }
        }]
    })
}

pub(crate) fn classify_ruleset_create_error(err: &str) -> RulesetCreateKind {
    let lower = err.to_ascii_lowercase();
    if lower.contains("already been taken") || lower.contains("name already exists") {
        return RulesetCreateKind::NameTaken;
    }
    if lower.contains("not a valid check")
        || lower.contains("required to be present is not found")
        || (lower.contains("422") && lower.contains("status check") && lower.contains("not found"))
    {
        return RulesetCreateKind::CheckNeverRun;
    }
    if lower.contains("(500")
        || lower.contains("(502")
        || lower.contains("(503")
        || lower.contains("(504")
        || lower.contains("(429")
    {
        return RulesetCreateKind::Retryable;
    }
    // "(401" / "(403" is the status token, a 422 body can contain those digits
    if lower.contains("(403")
        || lower.contains("(401")
        || lower.contains("upgrade to github pro")
        || lower.contains("not accessible")
    {
        return RulesetCreateKind::Refused;
    }
    // timeout and transport errors never include an HTTP status
    if !has_http_status_token(&lower) {
        return RulesetCreateKind::Retryable;
    }
    RulesetCreateKind::Other
}

fn has_http_status_token(err: &str) -> bool {
    err.match_indices('(').any(|(i, _)| {
        err.as_bytes()
            .get(i + 1..i + 4)
            .is_some_and(|digits| digits.iter().all(u8::is_ascii_digit))
    })
}

/// Classic protection prefers `checks` (includes `app_id`) and falls back to `contexts`
fn parse_classic_required_status_checks(value: &serde_json::Value) -> Vec<RequiredStatusCheck> {
    if let Some(checks) = value
        .pointer("/required_status_checks/checks")
        .and_then(|v| v.as_array())
    {
        let parsed: Vec<RequiredStatusCheck> =
            checks.iter().filter_map(parse_classic_check).collect();
        if !parsed.is_empty() {
            return parsed;
        }
    }

    value
        .pointer("/required_status_checks/contexts")
        .and_then(|v| v.as_array())
        .map(|contexts| {
            contexts
                .iter()
                .filter_map(|context| {
                    context.as_str().map(|s| RequiredStatusCheck {
                        context: s.to_string(),
                        integration_id: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_classic_check(check: &serde_json::Value) -> Option<RequiredStatusCheck> {
    let context = check.get("context")?.as_str()?.to_string();
    let integration_id = check
        .get("app_id")
        .and_then(serde_json::Value::as_i64)
        .filter(|&id| id > 0);
    Some(RequiredStatusCheck {
        context,
        integration_id,
    })
}

/// Collect `required_status_checks` entries from every active `required_status_checks` rule
fn parse_ruleset_required_status_checks(rules: &[serde_json::Value]) -> Vec<RequiredStatusCheck> {
    let mut out = Vec::new();
    for rule in rules {
        if rule.get("type").and_then(|t| t.as_str()) != Some("required_status_checks") {
            continue;
        }
        let Some(checks) = rule
            .pointer("/parameters/required_status_checks")
            .or_else(|| rule.get("required_status_checks"))
            .and_then(|v| v.as_array())
        else {
            continue;
        };
        for check in checks {
            let Some(context) = check.get("context").and_then(|v| v.as_str()) else {
                continue;
            };
            let integration_id = check
                .get("integration_id")
                .and_then(serde_json::Value::as_i64)
                .filter(|&id| id > 0);
            out.push(RequiredStatusCheck {
                context: context.to_string(),
                integration_id,
            });
        }
    }
    out
}

fn merge_required_status_checks(
    mut checks: Vec<RequiredStatusCheck>,
    extra: Vec<RequiredStatusCheck>,
) -> Vec<RequiredStatusCheck> {
    checks.extend(extra);
    let mut seen = HashSet::new();
    checks.retain(|c| seen.insert((c.context.clone(), c.integration_id)));
    checks
}

fn sanitize_github_issue_numbers(issue_numbers: &[u64]) -> (Vec<u64>, Vec<u64>) {
    let mut valid = Vec::with_capacity(issue_numbers.len());
    let mut invalid = Vec::new();
    for &issue_number in issue_numbers {
        if issue_number == 0 {
            invalid.push(issue_number);
        } else {
            valid.push(issue_number);
        }
    }
    (valid, invalid)
}

// ── Public metrics ───────────────────────────────────────────────────

/// Snapshot of GitHub API rate-limit metrics.
#[derive(Debug, Clone)]
pub struct RateLimitMetrics {
    /// Total 403/429 rate-limit responses received (REST + GraphQL combined).
    pub hits: u64,
    /// Calls skipped because the quota was low or a backoff was active.
    pub skipped_calls: u64,
    /// Current REST remaining quota (None if not yet observed).
    pub rest_remaining: Option<u32>,
    /// UTC epoch second when the REST window resets (None if not yet observed).
    pub rest_reset_at: Option<u64>,
    /// Current GraphQL remaining quota (None if not yet observed).
    pub graphql_remaining: Option<u32>,
    /// Total 5xx server error responses received (REST + GraphQL combined).
    pub five_xx_hits: u64,
}

/// Return a snapshot of GitHub API rate-limit metrics.
pub fn rate_limit_metrics() -> RateLimitMetrics {
    let (rest_remaining, rest_reset_at) = REST_RATE_LIMIT
        .lock()
        .ok()
        .map_or((None, None), |rl| (rl.remaining, rl.reset_at));
    let graphql_remaining = GRAPHQL_RATE_LIMIT.lock().ok().and_then(|rl| rl.remaining);
    RateLimitMetrics {
        hits: METRIC_RATE_LIMIT_HITS.load(Ordering::Relaxed),
        skipped_calls: METRIC_PROACTIVE_THROTTLES.load(Ordering::Relaxed),
        rest_remaining,
        rest_reset_at,
        graphql_remaining,
        five_xx_hits: METRIC_5XX_HITS.load(Ordering::Relaxed),
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
            if start >= end {
                return None;
            }
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
        "status:approved" => "2ea44f",
        "status:needs_review" => "e4e669",
        "status:waiting" => "5319e7",
        _ => "c5def5",
    }
}

// ── Batch GraphQL helpers ─────────────────────────────────────────────

/// Data fetched for one PR by [`GhHttp::batch_fetch_pr_review_data`].
#[derive(Debug, Clone, Default)]
pub struct PrReviewBatchData {
    /// True when the pull request was merged.
    pub merged: bool,
    /// Head commit of the pull request, empty when GitHub did not return it.
    pub head_sha: String,
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
        r"pr{n}: pullRequest(number: {n}) {{
  merged
  mergeable
  headRefOid
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
    pageInfo {{ hasNextPage }}
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
        pageInfo {{ hasNextPage }}
      }}
    }}
    pageInfo {{ hasNextPage }}
  }}
  comments(first: 50) {{
    nodes {{
      databaseId
      author {{ login }}
      body
      createdAt
      updatedAt
      url
    }}
    pageInfo {{ hasNextPage endCursor }}
  }}
}}"
    )
}

fn build_pr_issue_comments_page_query(
    owner: &str,
    name: &str,
    pr_number: u64,
    after_cursor: &str,
) -> (String, serde_json::Value) {
    let query = r"query($owner: String!, $name: String!, $number: Int!, $after: String!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      comments(first: 100, after: $after) {
        nodes {
          databaseId
          author { login }
          body
          createdAt
          updatedAt
          url
        }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}"
    .to_string();
    let vars = serde_json::json!({
        "owner": owner,
        "name": name,
        "number": pr_number,
        "after": after_cursor,
    });
    (query, vars)
}

fn connection_has_next_page(page_info: Option<&serde_json::Value>) -> bool {
    page_info
        .and_then(|v| v.get("hasNextPage"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn connection_end_cursor(page_info: Option<&serde_json::Value>) -> Option<String> {
    if !connection_has_next_page(page_info) {
        return None;
    }

    page_info
        .and_then(|v| v.get("endCursor"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

fn parse_graphql_review(node: &serde_json::Value) -> Option<GitHubReview> {
    Some(GitHubReview {
        id: node.get("databaseId").and_then(serde_json::Value::as_u64)?,
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
    Some(GitHubReviewComment {
        id: node.get("databaseId").and_then(serde_json::Value::as_u64)?,
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
        line: node
            .get("line")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u32::try_from(v).ok()),
        original_line: node
            .get("originalLine")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u32::try_from(v).ok()),
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
        in_reply_to_id: node
            .pointer("/replyTo/databaseId")
            .and_then(serde_json::Value::as_u64),
        diff_hunk: node
            .get("diffHunk")
            .and_then(|v| v.as_str())
            .map(String::from),
    })
}

fn parse_graphql_issue_comment(node: &serde_json::Value) -> Option<GitHubComment> {
    Some(GitHubComment {
        id: node.get("databaseId").and_then(serde_json::Value::as_u64)?,
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
        author_association: None,
    })
}

#[cfg(test)]
mod tests;
