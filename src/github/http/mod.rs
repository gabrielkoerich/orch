//! Native `reqwest` HTTP client for the GitHub API — replaces `gh` CLI subprocesses.
//!
//! Uses a shared `reqwest::Client` with connection pooling, reads rate-limit
//! headers proactively, and supports concurrent requests via `tokio::join!`.
//!
//! Auth: uses the centralized [`TokenResolver`] which supports:
//! - `GH_TOKEN` / `GITHUB_TOKEN` environment variables
//! - `gh.auth.token` config value
//! - `gh auth token` CLI fallback (default — just run `gh auth login`)

mod checks;
mod issues;
mod pulls;

use super::token;
use crate::config;
use crate::engine::cooldown as engine_cooldown;
use crate::engine::cooldown;
use crate::security;
use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
pub(crate) use checks::{
    REQUIRED_CHECK_RULESET_NAME, RulesetCreateKind, classify_ruleset_create_error,
};
pub use pulls::{CommitMeta, PrReviewBatchData};
use reqwest::{Client, Response, StatusCode, header};
use serde::de::DeserializeOwned;
use std::any;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::error::Error as _;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::fmt::Write as _;
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

        // orch's own calls use the gh token, the App only reviews (src/engine/review/). The App
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
}

/// A branch protection read that means "no protection": 404 "Branch not protected", or the 403
/// True when `text` carries the HTTP status in the `failed (404 ...)` form that every request error
/// here uses, or `HTTP 404` from the CLI. The status code sits apart from the URL, so an issue
/// number such as 1404 does not match
pub fn has_status(text: &str, status: u16) -> bool {
    text.contains(&format!("failed ({status}")) || text.contains(&format!("HTTP {status}"))
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

#[cfg(test)]
mod tests;
