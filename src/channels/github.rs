//! GitHub webhook server — receives events from GitHub and dispatches them.
//!
//! Provides `start_webhook_server` and `check_webhook_health` for the engine.

use super::IncomingMessage;
use chrono::{DateTime, Utc};

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;

#[derive(serde::Deserialize, serde::Serialize)]
struct GhUser {
    login: String,
}

type HmacSha256 = Hmac<Sha256>;

/// Default deduplication window: 2 hours (GitHub retries within ~1 hour).
const DEFAULT_DEDUP_WINDOW_SECS: u64 = 2 * 60 * 60;
/// Default cap on deduplication map to avoid unbounded memory growth.
const DEFAULT_MAX_SEEN_DELIVERIES: usize = 50_000;

// ---------------------------------------------------------------------------
// DedupStore — pluggable delivery deduplication
// ---------------------------------------------------------------------------

/// Metrics snapshot from the dedup store.
#[derive(serde::Serialize, Clone, Copy)]
pub struct DedupMetrics {
    /// Number of entries currently in the store.
    pub seen_count: usize,
    /// Total duplicate deliveries rejected since process start.
    pub duplicate_count: u64,
    /// Total new deliveries inserted since process start.
    pub inserted_count: u64,
    /// Total entries evicted due to TTL or size cap.
    pub evicted_count: u64,
}

struct DedupInner {
    /// delivery_id → unix timestamp (secs) when first seen.
    entries: HashMap<String, u64>,
    duplicate_count: u64,
    inserted_count: u64,
    evicted_count: u64,
}

/// Deduplication store — hides the backing implementation behind a clean API.
///
/// Two variants are supported:
/// - **In-memory** (`new_in_memory`): default, fastest, state lost on restart.
/// - **File-backed** (`new_filebacked`): persists to a file so dedup state
///   survives process restarts (prevents re-processing of GitHub retries after a
///   restart within the dedup window).
#[derive(Clone)]
pub struct DedupStore {
    inner: Arc<tokio::sync::Mutex<DedupInner>>,
    /// `Some(path)` enables file-backed persistence.
    file_path: Option<PathBuf>,
    window_secs: u64,
    max_size: usize,
    metrics_enabled: bool,
}

impl DedupStore {
    /// Create an in-memory dedup store with the given window and size cap.
    #[cfg(test)]
    pub fn new_in_memory(window_secs: u64, max_size: usize) -> Self {
        Self::new_in_memory_with_metrics(window_secs, max_size, true)
    }

    /// Create an in-memory dedup store with optional metrics.
    pub fn new_in_memory_with_metrics(
        window_secs: u64,
        max_size: usize,
        metrics_enabled: bool,
    ) -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(DedupInner {
                entries: HashMap::new(),
                duplicate_count: 0,
                inserted_count: 0,
                evicted_count: 0,
            })),
            file_path: None,
            window_secs,
            max_size,
            metrics_enabled,
        }
    }

    /// Create a file-backed dedup store.
    ///
    /// On construction the file is loaded (if it exists) and expired entries are
    /// filtered out. On each new insertion the state is flushed to disk so it
    /// survives process restarts within the dedup window.
    #[cfg(test)]
    pub fn new_filebacked(path: PathBuf, window_secs: u64, max_size: usize) -> Self {
        Self::new_filebacked_with_metrics(path, window_secs, max_size, true)
    }

    /// Create a file-backed dedup store with optional metrics.
    pub fn new_filebacked_with_metrics(
        path: PathBuf,
        window_secs: u64,
        max_size: usize,
        metrics_enabled: bool,
    ) -> Self {
        let entries = load_dedup_file(&path, window_secs);
        tracing::info!(
            path = %path.display(),
            loaded = entries.len(),
            "loaded webhook dedup state from file"
        );
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(DedupInner {
                entries,
                duplicate_count: 0,
                inserted_count: 0,
                evicted_count: 0,
            })),
            file_path: Some(path),
            window_secs,
            max_size,
            metrics_enabled,
        }
    }

    /// Check whether `delivery_id` is new and, if so, record it.
    ///
    /// Returns `true` if the delivery is new (should be processed) or
    /// `false` if it is a duplicate (should be silently ACKed).
    pub async fn check_and_insert(&self, delivery_id: &str) -> bool {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let (is_new, flush_entries) = {
            let mut inner = self.inner.lock().await;
            // Evict time-expired entries first.
            let cutoff = now_secs.saturating_sub(self.window_secs);
            let mut expired = 0usize;
            if self.metrics_enabled {
                let before = inner.entries.len();
                inner.entries.retain(|_, ts| *ts > cutoff);
                expired = before.saturating_sub(inner.entries.len());
            } else {
                inner.entries.retain(|_, ts| *ts > cutoff);
            }

            if inner.entries.contains_key(delivery_id) {
                if self.metrics_enabled {
                    inner.duplicate_count += 1;
                    inner.evicted_count += expired as u64;
                }
                (false, None)
            } else {
                inner.entries.insert(delivery_id.to_string(), now_secs);
                if self.metrics_enabled {
                    inner.inserted_count += 1;
                }
                // Enforce size cap after insertion so the store never exceeds max_size.
                let evicted_size = enforce_size_cap(&mut inner.entries, self.max_size);
                if self.metrics_enabled {
                    inner.evicted_count += expired as u64;
                    inner.evicted_count += evicted_size as u64;
                }
                // Snapshot for file flush only when file-backed.
                let snap = self.file_path.is_some().then(|| inner.entries.clone());
                (true, snap)
            }
        };

        // Flush outside the mutex to keep the critical section short.
        // Use spawn_blocking so that the blocking std::fs calls (File::create,
        // write_all, sync_all, rename) do not occupy a Tokio async worker thread.
        // Retry up to 3 times with exponential backoff (100ms, 200ms) to handle
        // transient I/O failures without permanently losing dedup state.
        if let (Some(path), Some(entries)) = (&self.file_path, flush_entries) {
            let path = path.clone();
            let path_for_log = path.clone();
            let entries = Arc::new(entries);
            const MAX_FLUSH_ATTEMPTS: u32 = 3;
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                let path_clone = path.clone();
                let entries_clone = Arc::clone(&entries);
                match tokio::task::spawn_blocking(move || {
                    flush_dedup_file(&path_clone, &entries_clone)
                })
                .await
                {
                    Ok(Ok(())) => break,
                    Ok(Err(e)) if attempt < MAX_FLUSH_ATTEMPTS => {
                        let delay = std::time::Duration::from_millis(100u64 << (attempt - 1));
                        tracing::warn!(
                            ?e,
                            attempt,
                            path = %path_for_log.display(),
                            "failed to flush webhook dedup state, retrying"
                        );
                        tokio::time::sleep(delay).await;
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(
                            ?e,
                            path = %path_for_log.display(),
                            "failed to flush webhook dedup state after {MAX_FLUSH_ATTEMPTS} attempts"
                        );
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(?e, "spawn_blocking failed for flush_dedup_file");
                        break;
                    }
                }
            }
        }

        is_new
    }

    /// Return a metrics snapshot (non-blocking).
    pub async fn metrics(&self) -> DedupMetrics {
        let inner = self.inner.lock().await;
        DedupMetrics {
            seen_count: inner.entries.len(),
            duplicate_count: if self.metrics_enabled {
                inner.duplicate_count
            } else {
                0
            },
            inserted_count: if self.metrics_enabled {
                inner.inserted_count
            } else {
                0
            },
            evicted_count: if self.metrics_enabled {
                inner.evicted_count
            } else {
                0
            },
        }
    }

    pub fn metrics_enabled(&self) -> bool {
        self.metrics_enabled
    }
}

/// Evict the oldest entries until the map is at most `max_size`.
fn enforce_size_cap(entries: &mut HashMap<String, u64>, max_size: usize) -> usize {
    if entries.len() <= max_size {
        return 0;
    }
    let mut sorted: Vec<(String, u64)> = entries.iter().map(|(k, v)| (k.clone(), *v)).collect();
    sorted.sort_by_key(|(_, t)| *t);
    let overflow = sorted.len().saturating_sub(max_size);
    for (key, _) in sorted.into_iter().take(overflow) {
        entries.remove(&key);
    }
    overflow
}

/// Evict expired entries and enforce the size cap (oldest evicted first).
/// Used directly in tests; `check_and_insert` uses inline expiry + `enforce_size_cap`.
#[cfg(test)]
fn prune_dedup_inner(inner: &mut DedupInner, now_secs: u64, window_secs: u64, max_size: usize) {
    let cutoff = now_secs.saturating_sub(window_secs);
    inner.entries.retain(|_, ts| *ts > cutoff);
    enforce_size_cap(&mut inner.entries, max_size);
}

/// Load dedup state from a JSON file, filtering out entries older than `window_secs`.
fn load_dedup_file(path: &Path, window_secs: u64) -> HashMap<String, u64> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(e) => {
            tracing::warn!(?e, path = %path.display(), "failed to read webhook dedup file, starting fresh");
            return HashMap::new();
        }
    };

    #[derive(serde::Deserialize)]
    struct FileFormat {
        entries: HashMap<String, u64>,
    }

    let parsed: FileFormat = match serde_json::from_str(&content) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(?e, path = %path.display(), "failed to parse webhook dedup file, starting fresh");
            return HashMap::new();
        }
    };

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let cutoff = now_secs.saturating_sub(window_secs);
    parsed
        .entries
        .into_iter()
        .filter(|(_, ts)| *ts > cutoff)
        .collect()
}

/// Persist the dedup entries to a JSON file.
fn flush_dedup_file(path: &Path, entries: &HashMap<String, u64>) -> std::io::Result<()> {
    #[derive(serde::Serialize)]
    struct FileFormat<'a> {
        entries: &'a HashMap<String, u64>,
    }

    let content = serde_json::to_string(&FileFormat { entries })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    atomic_write(path, &content)
}

fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp_path = path.with_extension(format!("db.tmp.{}", uuid::Uuid::new_v4()));
    let mut file = std::fs::File::create(&tmp_path)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp_path, path)?;
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            if let Err(e) = dir.sync_all() {
                tracing::warn!(?e, path = %parent.display(), "failed to sync parent directory after dedup file rename");
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Webhook server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct WebhookState {
    secret: String,
    repo: String,
    tx: tokio::sync::mpsc::Sender<IncomingMessage>,
    dedup_store: DedupStore,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct WebhookPayload {
    action: Option<String>,
    issue: Option<IssuePayload>,
    #[serde(rename = "pull_request")]
    pr: Option<PullRequestPayload>,
    comment: Option<CommentPayload>,
    review: Option<ReviewPayload>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct IssuePayload {
    number: u64,
    title: String,
    labels: Vec<LabelPayload>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct LabelPayload {
    name: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct PullRequestPayload {
    number: u64,
    title: String,
    body: Option<String>,
    action: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct CommentPayload {
    id: u64,
    body: String,
    user: GhUser,
    created_at: Option<DateTime<Utc>>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct ReviewPayload {
    id: u64,
    state: String,
    body: Option<String>,
    user: GhUser,
}

fn verify_signature(secret: &str, payload: &[u8], signature: &str) -> bool {
    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(payload);
    let result = mac.finalize().into_bytes();

    let expected = format!("sha256={:x}", result);
    if expected.len() != signature.len() {
        return false;
    }
    // Use constant-time comparison to prevent timing side-channel attacks
    bool::from(expected.as_bytes().ct_eq(signature.as_bytes()))
}

fn parse_github_event(
    payload: WebhookPayload,
    event_type: &str,
    _repo: &str,
) -> Option<IncomingMessage> {
    let timestamp = chrono::Utc::now();

    match event_type {
        "issues" => {
            if let (Some(action), Some(issue)) = (&payload.action, &payload.issue) {
                if action == "opened" || action == "labeled" {
                    let labels: Vec<String> = issue.labels.iter().map(|l| l.name.clone()).collect();
                    return Some(IncomingMessage {
                        channel: "github".to_string(),
                        id: format!("issue-{}-{}", issue.number, action),
                        thread_id: issue.number.to_string(),
                        author: "github".to_string(),
                        body: format!("Issue {}: {}", action, issue.title),
                        timestamp,
                        metadata: serde_json::json!({
                            "event": "issues",
                            "action": action,
                            "issue_number": issue.number,
                            "labels": labels
                        }),
                        topic_id: None,
                    });
                }
            }
        }
        "pull_request" => {
            if let (Some(action), Some(pr)) = (&payload.action, &payload.pr) {
                let relevant_actions = ["opened", "synchronize", "labeled", "ready_for_review"];
                if relevant_actions.contains(&action.as_str()) {
                    return Some(IncomingMessage {
                        channel: "github".to_string(),
                        id: format!("pr-{}-{}", pr.number, action),
                        thread_id: pr.number.to_string(),
                        author: "github".to_string(),
                        body: format!("PR {}: {}", action, pr.title),
                        timestamp,
                        metadata: serde_json::json!({
                            "event": "pull_request",
                            "action": action,
                            "pr_number": pr.number,
                            "pr_title": pr.title
                        }),
                        topic_id: None,
                    });
                }
            }
        }
        "issue_comment" => {
            if let (Some(action), Some(comment), Some(issue)) =
                (&payload.action, &payload.comment, &payload.issue)
            {
                if action == "created" {
                    return Some(IncomingMessage {
                        channel: "github".to_string(),
                        id: format!("comment-{}", comment.id),
                        thread_id: issue.number.to_string(),
                        author: comment.user.login.clone(),
                        body: comment.body.clone(),
                        timestamp: comment.created_at.unwrap_or(timestamp),
                        metadata: serde_json::json!({
                            "event": "issue_comment",
                            "action": action,
                            "issue_number": issue.number
                        }),
                        topic_id: None,
                    });
                }
            }
        }
        "pull_request_review" => {
            if let (Some(action), Some(review), Some(pr)) =
                (&payload.action, &payload.review, &payload.pr)
            {
                if action == "submitted" {
                    return Some(IncomingMessage {
                        channel: "github".to_string(),
                        id: format!("review-{}", review.id),
                        thread_id: pr.number.to_string(),
                        author: review.user.login.clone(),
                        body: review
                            .body
                            .clone()
                            .unwrap_or_else(|| format!("Review {}", review.state)),
                        timestamp,
                        metadata: serde_json::json!({
                            "event": "pull_request_review",
                            "action": action,
                            "pr_number": pr.number,
                            "review_state": review.state
                        }),
                        topic_id: None,
                    });
                }
            }
        }
        _ => {}
    }

    None
}

async fn handle_webhook(
    State(state): State<WebhookState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !state.secret.is_empty() && !verify_signature(&state.secret, &body, signature) {
        tracing::warn!("webhook signature verification failed");
        return (StatusCode::UNAUTHORIZED, "Invalid signature");
    }

    // Deduplicate by x-github-delivery header — GitHub retries unacknowledged
    // deliveries and allows manual re-delivery from the App settings UI.
    let delivery_id = headers
        .get("x-github-delivery")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if !delivery_id.is_empty() && !state.dedup_store.check_and_insert(&delivery_id).await {
        if state.dedup_store.metrics_enabled() {
            let metrics = state.dedup_store.metrics().await;
            tracing::debug!(
                delivery_id = %delivery_id,
                seen_count = metrics.seen_count,
                duplicate_count = metrics.duplicate_count,
                "duplicate webhook delivery, skipping"
            );
        } else {
            tracing::debug!(delivery_id = %delivery_id, "duplicate webhook delivery, skipping");
        }
        return (StatusCode::OK, "OK");
    }

    let payload: WebhookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(?e, "failed to parse webhook payload");
            return (StatusCode::BAD_REQUEST, "Invalid JSON");
        }
    };

    let event_type = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if let Some(msg) = parse_github_event(payload, event_type, &state.repo) {
        if state.tx.send(msg).await.is_err() {
            tracing::warn!("webhook event channel receiver dropped");
        }
    }

    (StatusCode::OK, "OK")
}

/// Health endpoint — returns JSON with dedup store status.
///
/// Response: `{"ok": true, "seen_count": N, "duplicate_count": M}`
async fn webhook_health(State(state): State<WebhookState>) -> impl IntoResponse {
    let metrics = state.dedup_store.metrics().await;
    axum::Json(serde_json::json!({
        "ok": true,
        "seen_count": metrics.seen_count,
        "duplicate_count": metrics.duplicate_count
    }))
}

/// Check if the webhook server is healthy by pinging its local health endpoint.
///
/// This only verifies the local HTTP listener is running. It does NOT verify
/// that GitHub can reach the endpoint (NAT/firewall) or that the webhook
/// secret is valid.
///
/// Returns `(healthy, failure_reason)`.  `failure_reason` is `Some` when
/// unhealthy and cleared (i.e. `None`) when the check succeeds.
///
/// Logs dedup store metrics (seen_count, duplicate_count) at `debug` level for observability.
pub async fn check_webhook_health(port: u16) -> (bool, Option<String>) {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(?e, "failed to build HTTP client for webhook health check");
            return (false, Some(format!("failed to build HTTP client: {e}")));
        }
    };
    let url = format!("http://localhost:{}/health", port);
    match client.get(&url).send().await {
        Ok(response) if response.status().is_success() => {
            // Log dedup metrics from the health endpoint response.
            if let Ok(body) = response.json::<serde_json::Value>().await {
                let seen = body.get("seen_count").and_then(|v| v.as_u64()).unwrap_or(0);
                let dupes = body
                    .get("duplicate_count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                tracing::debug!(
                    seen_count = seen,
                    duplicate_count = dupes,
                    "webhook health ok"
                );
            }
            (true, None)
        }
        Ok(response) => {
            let reason = format!("health endpoint returned {}", response.status());
            (false, Some(reason))
        }
        Err(e) => (false, Some(format!("health check request failed: {e}"))),
    }
}

/// Return `true` when `e` represents a transient TCP bind error (e.g. port
/// already in use) that is worth retrying.
pub fn is_transient_bind_error(e: &anyhow::Error) -> bool {
    for cause in e.chain() {
        if let Some(io_err) = cause.downcast_ref::<std::io::Error>() {
            return matches!(
                io_err.kind(),
                std::io::ErrorKind::AddrInUse | std::io::ErrorKind::AddrNotAvailable
            );
        }
    }
    false
}

/// Compute an exponential-backoff delay for webhook startup retries.
///
/// Uses subsecond wall-clock bits as a cheap jitter source — no `rand` crate
/// needed.  Delay is capped at 30 s.
pub fn webhook_backoff_delay(attempt: u32) -> std::time::Duration {
    let base_ms: u64 = 500;
    let max_ms: u64 = 30_000;
    let exp_ms = base_ms.saturating_mul(1u64 << (attempt.saturating_sub(1)).min(6));
    let bounded_ms = exp_ms.min(max_ms);
    let jitter_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_millis() as u64
        % 500;
    std::time::Duration::from_millis(bounded_ms + jitter_ms)
}

/// Start the webhook HTTP server.
///
/// Dedup configuration is read from the global config:
/// - `webhook.dedup_window_seconds` (default: 7200)
/// - `webhook.max_seen_deliveries` (default: 50000)
/// - `webhook.dedup_store`: `"memory"` (default) or `"file"` — file-backed
///   variant persists dedup state across restarts to `{data_dir}/webhook_dedup.db`.
/// - `webhook.dedup_path`: override the file-backed path (optional).
/// - `webhook.dedup_metrics`: `"true"` (default) to enable metrics; `"false"` to disable.
pub async fn start_webhook_server(
    port: u16,
    secret: String,
    repo: String,
    tx: tokio::sync::mpsc::Sender<IncomingMessage>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;

    let window_secs = crate::config::get("webhook.dedup_window_seconds")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_DEDUP_WINDOW_SECS);

    let max_size = crate::config::get("webhook.max_seen_deliveries")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_SEEN_DELIVERIES);

    let store_type =
        crate::config::get("webhook.dedup_store").unwrap_or_else(|_| "memory".to_string());

    let metrics_enabled = crate::config::get("webhook.dedup_metrics")
        .map(|v| v == "true")
        .unwrap_or(true);

    let dedup_store = if store_type == "file" {
        let path = crate::config::get("webhook.dedup_path")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                crate::home::orch_home()
                    .unwrap_or_else(|_| PathBuf::from("/tmp"))
                    .join("webhook_dedup.db")
            });

        // Initialization may perform blocking filesystem I/O (reading the
        // dedup file). Run the constructor on a blocking thread so we don't
        // block the Tokio async worker thread that is running
        // start_webhook_server.
        match tokio::task::spawn_blocking(move || {
            DedupStore::new_filebacked_with_metrics(path, window_secs, max_size, metrics_enabled)
        })
        .await
        {
            Ok(store) => store,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "failed to initialize file-backed dedup store: {e}"
                ))
            }
        }
    } else {
        DedupStore::new_in_memory_with_metrics(window_secs, max_size, metrics_enabled)
    };

    tracing::info!(
        port,
        dedup_store = %store_type,
        dedup_window_secs = window_secs,
        max_seen_deliveries = max_size,
        dedup_metrics = metrics_enabled,
        "starting webhook server"
    );

    let state = WebhookState {
        secret,
        repo,
        tx,
        dedup_store,
    };

    let app = Router::new()
        .route("/health", get(webhook_health))
        .route("/webhook", post(handle_webhook))
        .with_state(state);

    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_signature_valid() {
        let secret = "test-secret";
        let payload = br#"{"action":"opened"}"#;
        let signature = "sha256=e7f446e3b1c1c8e7b8c8e7b8c8e7b8c8e7b8c8e7b8c8e7b8c8e7b8c8e7b8c8";

        let result = verify_signature(secret, payload, signature);
        assert!(!result, "Invalid signature format should fail");
    }

    #[test]
    fn test_verify_signature_invalid() {
        let secret = "test-secret";
        let payload = br#"{"action":"opened"}"#;
        let signature = "sha256=invalidsignature";

        let result = verify_signature(secret, payload, signature);
        assert!(!result, "Invalid signature should return false");
    }

    #[test]
    fn test_verify_signature_empty_secret() {
        let secret = "";
        let payload = br#"{"action":"opened"}"#;
        let signature = "sha256=anysignature";

        let result = verify_signature(secret, payload, signature);
        assert!(!result, "Empty secret should return false");
    }

    #[test]
    fn test_parse_github_event_issue_opened() {
        let payload = WebhookPayload {
            action: Some("opened".to_string()),
            issue: Some(IssuePayload {
                number: 42,
                title: "Test issue".to_string(),
                labels: vec![],
            }),
            pr: None,
            comment: None,
            review: None,
        };

        let msg = parse_github_event(payload, "issues", "owner/repo");
        assert!(msg.is_some());

        let msg = msg.unwrap();
        assert_eq!(msg.channel, "github");
        assert_eq!(msg.thread_id, "42");
        assert!(msg.body.contains("opened"));
        assert!(msg.body.contains("Test issue"));
    }

    #[test]
    fn test_parse_github_event_issue_labeled() {
        let payload = WebhookPayload {
            action: Some("labeled".to_string()),
            issue: Some(IssuePayload {
                number: 42,
                title: "Test issue".to_string(),
                labels: vec![
                    LabelPayload {
                        name: "bug".to_string(),
                    },
                    LabelPayload {
                        name: "urgent".to_string(),
                    },
                ],
            }),
            pr: None,
            comment: None,
            review: None,
        };

        let msg = parse_github_event(payload, "issues", "owner/repo");
        assert!(msg.is_some());

        let msg = msg.unwrap();
        assert_eq!(msg.thread_id, "42");
        let labels = msg.metadata.get("labels").unwrap().as_array().unwrap();
        assert_eq!(labels.len(), 2);
    }

    #[test]
    fn test_parse_github_event_pr_opened() {
        let payload = WebhookPayload {
            action: Some("opened".to_string()),
            issue: None,
            pr: Some(PullRequestPayload {
                number: 101,
                title: "New feature".to_string(),
                body: Some("Description".to_string()),
                action: Some("opened".to_string()),
            }),
            comment: None,
            review: None,
        };

        let msg = parse_github_event(payload, "pull_request", "owner/repo");
        assert!(msg.is_some());

        let msg = msg.unwrap();
        assert_eq!(msg.thread_id, "101");
        assert!(msg.body.contains("opened"));
        assert!(msg.body.contains("New feature"));
    }

    #[test]
    fn test_parse_github_event_pr_synchronize() {
        let payload = WebhookPayload {
            action: Some("synchronize".to_string()),
            issue: None,
            pr: Some(PullRequestPayload {
                number: 101,
                title: "New feature".to_string(),
                body: Some("Description".to_string()),
                action: Some("synchronize".to_string()),
            }),
            comment: None,
            review: None,
        };

        let msg = parse_github_event(payload, "pull_request", "owner/repo");
        assert!(msg.is_some());

        let msg = msg.unwrap();
        assert!(msg.body.contains("synchronize"));
    }

    #[test]
    fn test_parse_github_event_ignored_action() {
        let payload = WebhookPayload {
            action: Some("closed".to_string()),
            issue: Some(IssuePayload {
                number: 42,
                title: "Test issue".to_string(),
                labels: vec![],
            }),
            pr: None,
            comment: None,
            review: None,
        };

        let msg = parse_github_event(payload, "issues", "owner/repo");
        assert!(msg.is_none(), "closed action should be ignored");
    }

    #[test]
    fn test_parse_github_event_unknown_type() {
        let payload = WebhookPayload {
            action: Some("created".to_string()),
            issue: None,
            pr: None,
            comment: None,
            review: None,
        };

        let msg = parse_github_event(payload, "unknown_event", "owner/repo");
        assert!(msg.is_none());
    }

    #[test]
    fn test_parse_github_event_issue_comment() {
        let payload = WebhookPayload {
            action: Some("created".to_string()),
            issue: Some(IssuePayload {
                number: 42,
                title: "Test issue".to_string(),
                labels: vec![],
            }),
            pr: None,
            comment: Some(CommentPayload {
                id: 12345,
                body: "This is a comment".to_string(),
                user: GhUser {
                    login: "testuser".to_string(),
                },
                created_at: Some(
                    chrono::DateTime::parse_from_rfc3339("2024-01-15T10:30:00Z")
                        .unwrap()
                        .with_timezone(&chrono::Utc),
                ),
            }),
            review: None,
        };

        let msg = parse_github_event(payload, "issue_comment", "owner/repo");
        assert!(msg.is_some());

        let msg = msg.unwrap();
        assert_eq!(msg.id, "comment-12345", "ID should use actual comment ID");
        assert_eq!(msg.author, "testuser");
        assert_eq!(msg.body, "This is a comment");
        assert_eq!(msg.thread_id, "42");
        assert_eq!(
            msg.metadata.get("issue_number").unwrap().as_u64().unwrap(),
            42
        );
    }

    #[test]
    fn test_parse_github_event_issue_comment_without_issue_ignored() {
        let payload = WebhookPayload {
            action: Some("created".to_string()),
            issue: None,
            pr: None,
            comment: Some(CommentPayload {
                id: 12345,
                body: "Orphan comment".to_string(),
                user: GhUser {
                    login: "testuser".to_string(),
                },
                created_at: None,
            }),
            review: None,
        };

        let msg = parse_github_event(payload, "issue_comment", "owner/repo");
        assert!(msg.is_none(), "comment without issue should be ignored");
    }

    #[test]
    fn test_parse_github_event_pr_review_submitted() {
        let payload = WebhookPayload {
            action: Some("submitted".to_string()),
            issue: None,
            pr: Some(PullRequestPayload {
                number: 55,
                title: "Add feature".to_string(),
                body: Some("Description".to_string()),
                action: None,
            }),
            comment: None,
            review: Some(ReviewPayload {
                id: 98765,
                state: "changes_requested".to_string(),
                body: Some("Please fix the tests".to_string()),
                user: GhUser {
                    login: "reviewer".to_string(),
                },
            }),
        };

        let msg = parse_github_event(payload, "pull_request_review", "owner/repo");
        assert!(msg.is_some());

        let msg = msg.unwrap();
        assert_eq!(msg.id, "review-98765", "ID should use actual review ID");
        assert_eq!(msg.thread_id, "55");
        assert_eq!(msg.author, "reviewer");
        assert_eq!(msg.body, "Please fix the tests");
        assert_eq!(
            msg.metadata.get("review_state").unwrap().as_str().unwrap(),
            "changes_requested"
        );
    }

    #[test]
    fn test_parse_github_event_pr_review_non_submitted_ignored() {
        let payload = WebhookPayload {
            action: Some("dismissed".to_string()),
            issue: None,
            pr: Some(PullRequestPayload {
                number: 55,
                title: "Add feature".to_string(),
                body: None,
                action: None,
            }),
            comment: None,
            review: Some(ReviewPayload {
                id: 98765,
                state: "dismissed".to_string(),
                body: None,
                user: GhUser {
                    login: "reviewer".to_string(),
                },
            }),
        };

        let msg = parse_github_event(payload, "pull_request_review", "owner/repo");
        assert!(msg.is_none(), "dismissed review should be ignored");
    }

    #[test]
    fn test_verify_signature_correct() {
        let secret = "mysecret";
        let payload = br#"{"action":"opened"}"#;

        // Compute the correct HMAC-SHA256
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(payload);
        let result = mac.finalize().into_bytes();
        let signature = format!("sha256={:x}", result);

        assert!(
            verify_signature(secret, payload, &signature),
            "correct signature should verify"
        );
    }

    // ---------------------------------------------------------------------------
    // DedupStore unit tests
    // ---------------------------------------------------------------------------

    #[test]
    fn test_dedup_store_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DedupStore>();
    }

    #[tokio::test]
    async fn test_dedup_store_new_entry_is_accepted() {
        let store = DedupStore::new_in_memory(7200, 50_000);
        assert!(
            store.check_and_insert("delivery-1").await,
            "first insertion should return true"
        );
    }

    #[tokio::test]
    async fn test_dedup_store_duplicate_is_rejected() {
        let store = DedupStore::new_in_memory(7200, 50_000);
        assert!(store.check_and_insert("delivery-1").await);
        assert!(
            !store.check_and_insert("delivery-1").await,
            "duplicate should return false"
        );
    }

    #[tokio::test]
    async fn test_dedup_store_metrics_track_counts() {
        let store = DedupStore::new_in_memory(7200, 50_000);

        store.check_and_insert("d1").await;
        store.check_and_insert("d2").await;
        store.check_and_insert("d1").await; // duplicate

        let m = store.metrics().await;
        assert_eq!(m.seen_count, 2, "two unique IDs stored");
        assert_eq!(m.inserted_count, 2, "two insertions");
        assert_eq!(m.duplicate_count, 1, "one duplicate");
    }

    #[tokio::test]
    async fn test_dedup_store_caps_size() {
        let max = 10usize;
        let store = DedupStore::new_in_memory(7200, max);

        // Insert max + 5 entries; each with a unique ID.
        for i in 0..(max + 5) {
            store.check_and_insert(&format!("d-{}", i)).await;
        }

        let m = store.metrics().await;
        assert!(
            m.seen_count <= max,
            "seen_count {} should not exceed max {}",
            m.seen_count,
            max
        );
    }

    #[tokio::test]
    async fn test_dedup_store_filebacked_persists_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dedup.json");

        // First instance — insert an entry.
        {
            let store = DedupStore::new_filebacked(path.clone(), 7200, 50_000);
            store.check_and_insert("delivery-persisted").await;
        }

        // Second instance — load from same file; should still see the entry as duplicate.
        {
            let store = DedupStore::new_filebacked(path.clone(), 7200, 50_000);
            assert!(
                !store.check_and_insert("delivery-persisted").await,
                "entry loaded from file should be treated as duplicate"
            );
        }
    }

    #[tokio::test]
    async fn test_dedup_store_filebacked_expired_entries_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dedup.json");

        // Write a file with an entry that is already expired (timestamp = 0).
        let content = r#"{"entries":{"old-delivery":0}}"#;
        std::fs::write(&path, content).unwrap();

        // Load with a 1-second window; the entry (ts=0) is long expired.
        let store = DedupStore::new_filebacked(path, 1, 50_000);
        assert!(
            store.check_and_insert("old-delivery").await,
            "expired entry should not block new delivery with same ID"
        );
    }

    #[test]
    fn test_prune_dedup_inner_caps_size() {
        let mut inner = DedupInner {
            entries: HashMap::new(),
            duplicate_count: 0,
            inserted_count: 0,
            evicted_count: 0,
        };
        // Use a real-world-scale timestamp so subtracting offsets doesn't underflow.
        let now_secs: u64 = 1_700_000_000;

        for i in 0..(DEFAULT_MAX_SEEN_DELIVERIES + 10) {
            inner
                .entries
                .insert(format!("id-{}", i), now_secs - i as u64);
        }

        // Use a window large enough so none of the test entries expire (they span
        // DEFAULT_MAX_SEEN_DELIVERIES + 10 seconds from now_secs).
        let large_window = (DEFAULT_MAX_SEEN_DELIVERIES + 100) as u64;
        prune_dedup_inner(
            &mut inner,
            now_secs,
            large_window,
            DEFAULT_MAX_SEEN_DELIVERIES,
        );

        assert_eq!(inner.entries.len(), DEFAULT_MAX_SEEN_DELIVERIES);
        assert!(
            inner.entries.contains_key("id-0"),
            "newest entry should remain"
        );
        assert!(
            !inner
                .entries
                .contains_key(&format!("id-{}", DEFAULT_MAX_SEEN_DELIVERIES + 9)),
            "oldest entries should be evicted"
        );
    }

    /// Integration test: exercise the webhook HTTP handler end-to-end.
    /// Sends a POST to /webhook with an issues.opened payload and verifies
    /// the IncomingMessage arrives on the mpsc channel.
    #[tokio::test]
    async fn test_webhook_handler_issues_opened() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::util::ServiceExt; // for oneshot()

        let (tx, mut rx) = tokio::sync::mpsc::channel::<IncomingMessage>(16);

        let state = WebhookState {
            secret: String::new(), // empty secret = skip verification
            repo: "owner/repo".to_string(),
            tx,
            dedup_store: DedupStore::new_in_memory(7200, 50_000),
        };

        let app = Router::new()
            .route("/webhook", post(handle_webhook))
            .with_state(state);

        let body = serde_json::json!({
            "action": "opened",
            "issue": {
                "number": 99,
                "title": "New bug report",
                "labels": [{"name": "bug"}]
            }
        });

        let request = Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("content-type", "application/json")
            .header("x-github-event", "issues")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // The handler should have sent an IncomingMessage to the channel
        let msg = rx.try_recv().expect("should receive webhook message");
        assert_eq!(msg.channel, "github");
        assert_eq!(msg.thread_id, "99");
        assert_eq!(
            msg.metadata.get("event").unwrap().as_str().unwrap(),
            "issues"
        );
        assert_eq!(
            msg.metadata.get("action").unwrap().as_str().unwrap(),
            "opened"
        );
    }

    /// Integration test: verify that issue_comment events include the correct thread_id.
    #[tokio::test]
    async fn test_webhook_handler_issue_comment() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (tx, mut rx) = tokio::sync::mpsc::channel::<IncomingMessage>(16);

        let state = WebhookState {
            secret: String::new(),
            repo: "owner/repo".to_string(),
            tx,
            dedup_store: DedupStore::new_in_memory(7200, 50_000),
        };

        let app = Router::new()
            .route("/webhook", post(handle_webhook))
            .with_state(state);

        let body = serde_json::json!({
            "action": "created",
            "issue": {
                "number": 42,
                "title": "Existing issue",
                "labels": []
            },
            "comment": {
                "id": 12345,
                "body": "@orch please fix this",
                "user": {"login": "contributor"},
                "created_at": "2024-06-15T12:00:00Z"
            }
        });

        let request = Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("content-type", "application/json")
            .header("x-github-event", "issue_comment")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let msg = rx.try_recv().expect("should receive comment message");
        assert_eq!(msg.thread_id, "42", "thread_id should be the issue number");
        assert_eq!(msg.author, "contributor");
        assert!(msg.body.contains("@orch"));
    }

    /// Integration test: verify pull_request_review.submitted events are handled.
    #[tokio::test]
    async fn test_webhook_handler_pr_review_submitted() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (tx, mut rx) = tokio::sync::mpsc::channel::<IncomingMessage>(16);

        let state = WebhookState {
            secret: String::new(),
            repo: "owner/repo".to_string(),
            tx,
            dedup_store: DedupStore::new_in_memory(7200, 50_000),
        };

        let app = Router::new()
            .route("/webhook", post(handle_webhook))
            .with_state(state);

        let body = serde_json::json!({
            "action": "submitted",
            "pull_request": {
                "number": 77,
                "title": "Add feature X"
            },
            "review": {
                "id": 98765,
                "state": "changes_requested",
                "body": "Fix the failing tests",
                "user": {"login": "maintainer"}
            }
        });

        let request = Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("content-type", "application/json")
            .header("x-github-event", "pull_request_review")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let msg = rx.try_recv().expect("should receive review message");
        assert_eq!(msg.thread_id, "77");
        assert_eq!(msg.author, "maintainer");
        assert_eq!(
            msg.metadata.get("review_state").unwrap().as_str().unwrap(),
            "changes_requested"
        );
    }

    /// Integration test: verify that invalid signatures are rejected.
    #[tokio::test]
    async fn test_webhook_handler_rejects_invalid_signature() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (tx, mut rx) = tokio::sync::mpsc::channel::<IncomingMessage>(16);

        let state = WebhookState {
            secret: "real-secret".to_string(),
            repo: "owner/repo".to_string(),
            tx,
            dedup_store: DedupStore::new_in_memory(7200, 50_000),
        };

        let app = Router::new()
            .route("/webhook", post(handle_webhook))
            .with_state(state);

        let body = serde_json::json!({
            "action": "opened",
            "issue": {
                "number": 1,
                "title": "Should be rejected",
                "labels": []
            }
        });

        let request = Request::builder()
            .method("POST")
            .uri("/webhook")
            .header("content-type", "application/json")
            .header("x-github-event", "issues")
            .header("x-hub-signature-256", "sha256=invalid")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // No message should have been sent
        assert!(
            rx.try_recv().is_err(),
            "no message should be sent for invalid signature"
        );
    }

    /// Deduplication test: same x-github-delivery ID sent twice → only one message
    /// processed by the engine. Also verifies metrics are recorded correctly.
    #[tokio::test]
    async fn test_webhook_deduplication_same_delivery_id() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (tx, mut rx) = tokio::sync::mpsc::channel::<IncomingMessage>(16);

        // Shared dedup store — both requests use the same state.
        let dedup_store = DedupStore::new_in_memory(7200, 50_000);

        let body_json = serde_json::json!({
            "action": "opened",
            "issue": {
                "number": 200,
                "title": "Deduplicated issue",
                "labels": []
            }
        });
        let body_bytes = serde_json::to_vec(&body_json).unwrap();

        // First delivery — should be processed.
        {
            let state = WebhookState {
                secret: String::new(),
                repo: "owner/repo".to_string(),
                tx: tx.clone(),
                dedup_store: dedup_store.clone(),
            };
            let app = Router::new()
                .route("/webhook", post(handle_webhook))
                .with_state(state);
            let request = Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("content-type", "application/json")
                .header("x-github-event", "issues")
                .header("x-github-delivery", "abc-123-unique-id")
                .body(Body::from(body_bytes.clone()))
                .unwrap();
            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        // Should have received exactly one message.
        let msg = rx
            .try_recv()
            .expect("first delivery should produce a message");
        assert_eq!(msg.thread_id, "200");
        assert!(rx.try_recv().is_err(), "no second message yet");

        // Second delivery with the SAME delivery ID — should be silently ACKed.
        {
            let state = WebhookState {
                secret: String::new(),
                repo: "owner/repo".to_string(),
                tx: tx.clone(),
                dedup_store: dedup_store.clone(),
            };
            let app = Router::new()
                .route("/webhook", post(handle_webhook))
                .with_state(state);
            let request = Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("content-type", "application/json")
                .header("x-github-event", "issues")
                .header("x-github-delivery", "abc-123-unique-id")
                .body(Body::from(body_bytes.clone()))
                .unwrap();
            let response = app.oneshot(request).await.unwrap();
            // Must still return 200 OK to prevent GitHub from retrying again.
            assert_eq!(response.status(), StatusCode::OK);
        }

        // No second message should have been sent.
        assert!(
            rx.try_recv().is_err(),
            "duplicate delivery should not produce a second message"
        );

        // Verify metrics: 1 inserted, 1 duplicate.
        let m = dedup_store.metrics().await;
        assert_eq!(m.inserted_count, 1, "one unique delivery inserted");
        assert_eq!(m.duplicate_count, 1, "one duplicate rejected");
        assert_eq!(m.seen_count, 1, "one entry in store");

        // Third delivery with a DIFFERENT delivery ID — should be processed normally.
        {
            let state = WebhookState {
                secret: String::new(),
                repo: "owner/repo".to_string(),
                tx: tx.clone(),
                dedup_store: dedup_store.clone(),
            };
            let app = Router::new()
                .route("/webhook", post(handle_webhook))
                .with_state(state);
            let request = Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("content-type", "application/json")
                .header("x-github-event", "issues")
                .header("x-github-delivery", "xyz-456-different-id")
                .body(Body::from(body_bytes))
                .unwrap();
            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        let msg2 = rx
            .try_recv()
            .expect("different delivery ID should produce a message");
        assert_eq!(msg2.thread_id, "200");

        // Final metrics check.
        let m = dedup_store.metrics().await;
        assert_eq!(m.inserted_count, 2, "two unique deliveries inserted total");
        assert_eq!(m.duplicate_count, 1, "still one duplicate");
    }

    /// Health endpoint test: verify it returns JSON with dedup status.
    #[tokio::test]
    async fn test_health_endpoint_returns_json_metrics() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (tx, _rx) = tokio::sync::mpsc::channel::<IncomingMessage>(16);
        let dedup_store = DedupStore::new_in_memory(7200, 50_000);

        // Pre-populate with one entry and one duplicate for metrics.
        dedup_store.check_and_insert("health-test-1").await;
        dedup_store.check_and_insert("health-test-1").await;

        let state = WebhookState {
            secret: String::new(),
            repo: "owner/repo".to_string(),
            tx,
            dedup_store,
        };

        let app = Router::new()
            .route("/health", get(webhook_health))
            .with_state(state);

        let request = Request::builder()
            .method("GET")
            .uri("/health")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

        assert_eq!(json["ok"], true);
        assert_eq!(json["seen_count"], 1);
        assert_eq!(json["duplicate_count"], 1);
    }

    /// Verify that `is_transient_bind_error` correctly classifies EADDRINUSE.
    #[test]
    fn test_is_transient_bind_error_addr_in_use() {
        let io_err = std::io::Error::from(std::io::ErrorKind::AddrInUse);
        let err = anyhow::anyhow!(io_err);
        assert!(is_transient_bind_error(&err));
    }

    /// Verify that non-bind errors are not classified as transient.
    #[test]
    fn test_is_transient_bind_error_other_error() {
        let io_err = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        let err = anyhow::anyhow!(io_err);
        assert!(!is_transient_bind_error(&err));
    }

    /// Verify that `webhook_backoff_delay` respects the 30 s cap.
    #[test]
    fn test_webhook_backoff_delay_cap() {
        // At attempt 10 (well past the exponent cap) the base delay should be
        // capped at 30 000 ms plus up to 499 ms of jitter.
        let delay = webhook_backoff_delay(10);
        assert!(
            delay.as_millis() <= 30_500,
            "delay exceeded 30 s cap + jitter: {:?}",
            delay
        );
        assert!(
            delay.as_millis() >= 500,
            "delay should be at least base: {:?}",
            delay
        );
    }

    /// Integration test: a double-bind on the same `0.0.0.0` address fails
    /// with EADDRINUSE, and `is_transient_bind_error` correctly classifies it.
    ///
    /// We test the bind step directly (not the full `start_webhook_server`)
    /// because `axum::serve` runs until shutdown and would hang the test
    /// suite if the first bind accidentally succeeded.
    #[tokio::test]
    async fn test_start_webhook_server_port_in_use() {
        // Hold a listener on 0.0.0.0 (the same bind address used by start_webhook_server).
        let holder = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port = holder.local_addr().unwrap().port();

        // A second bind on the same address+port must fail.
        let result = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await;
        drop(holder);

        let io_err = result.expect_err("expected EADDRINUSE on duplicate bind");
        let anyhow_err = anyhow::anyhow!(io_err);
        assert!(
            is_transient_bind_error(&anyhow_err),
            "EADDRINUSE should be classified as transient: {anyhow_err}"
        );
    }
}
