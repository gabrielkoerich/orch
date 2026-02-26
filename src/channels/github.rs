//! GitHub channel — receives commands from issue comments, sends updates.
//!
//! Polls for new comments on issues with `status:in_progress` labels.
//! Posts task updates, agent output summaries, and status changes as comments.

use super::{Channel, IncomingMessage, OutgoingMessage, OutputChunk};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use std::process::Command;
use tokio::sync::broadcast;

pub struct GitHubChannel {
    repo: String,
    client: reqwest::Client,
    last_comment_ids: std::sync::Arc<std::sync::Mutex<HashSet<String>>>,
}

#[derive(serde::Deserialize)]
struct GhIssue {
    number: u64,
}

#[derive(serde::Deserialize)]
struct GhComment {
    id: u64,
    body: String,
    user: GhUser,
    created_at: DateTime<Utc>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct GhUser {
    login: String,
}

impl GitHubChannel {
    pub fn new(repo: String) -> Self {
        Self {
            repo,
            client: reqwest::Client::new(),
            last_comment_ids: std::sync::Arc::new(std::sync::Mutex::new(HashSet::new())),
        }
    }

    fn gh_api(&self, endpoint: &str) -> String {
        format!("https://api.github.com{}", endpoint)
    }

    async fn fetch_in_progress_issues(&self) -> anyhow::Result<Vec<GhIssue>> {
        let url = self.gh_api(&format!(
            "/repos/{}/issues?labels=status:in_progress&state=all",
            self.repo
        ));

        let output = Command::new("gh")
            .args([
                "api",
                &url,
                "--header",
                "Accept: application/vnd.github.v3+json",
            ])
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api failed: {}", stderr);
        }

        let issues: Vec<GhIssue> = serde_json::from_slice(&output.stdout)?;
        Ok(issues)
    }

    async fn fetch_comments(&self, issue_number: u64) -> anyhow::Result<Vec<GhComment>> {
        let url = self.gh_api(&format!(
            "/repos/{}/issues/{}/comments",
            self.repo, issue_number
        ));

        let output = Command::new("gh")
            .args([
                "api",
                &url,
                "--header",
                "Accept: application/vnd.github.v3+json",
            ])
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh api failed: {}", stderr);
        }

        let comments: Vec<GhComment> = serde_json::from_slice(&output.stdout)?;
        Ok(comments)
    }

    fn post_comment(&self, issue_number: u64, body: &str) -> anyhow::Result<()> {
        let url = self.gh_api(&format!(
            "/repos/{}/issues/{}/comments",
            self.repo, issue_number
        ));

        let output = Command::new("gh")
            .args([
                "api",
                &url,
                "--header",
                "Accept: application/vnd.github.v3+json",
                "--method",
                "POST",
                "--field",
                &format!("body={}", body.replace('\n', "\\n")),
            ])
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("failed to post comment: {}", stderr);
        }

        Ok(())
    }
}

#[async_trait]
impl Channel for GitHubChannel {
    fn name(&self) -> &str {
        "github"
    }

    async fn start(&self) -> anyhow::Result<tokio::sync::mpsc::Receiver<IncomingMessage>> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let repo = self.repo.clone();
        let _client = self.client.clone();
        let last_comment_ids = self.last_comment_ids.clone();

        tracing::info!(repo = %repo, "github channel started (polling)");

        tokio::spawn(async move {
            let polling_interval = std::time::Duration::from_secs(30);
            let mut last_check = Utc::now();
            let repo_clone = repo.clone();

            loop {
                tokio::time::sleep(polling_interval).await;

                let issues = match GitHubChannel::new(repo_clone.clone())
                    .fetch_in_progress_issues()
                    .await
                {
                    Ok(i) => i,
                    Err(e) => {
                        tracing::warn!(?e, "failed to fetch in_progress issues");
                        continue;
                    }
                };

                for issue in issues {
                    let comments = match GitHubChannel::new(repo_clone.clone())
                        .fetch_comments(issue.number)
                        .await
                    {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(issue = issue.number, ?e, "failed to fetch comments");
                            continue;
                        }
                    };

                    let comment_ids: Vec<String> = {
                        let mut seen_ids = last_comment_ids.lock().unwrap();
                        let ids: Vec<String> = comments
                            .iter()
                            .map(|c| c.id.to_string())
                            .filter(|id| !seen_ids.contains(id))
                            .collect();
                        for id in &ids {
                            seen_ids.insert(id.clone());
                        }
                        ids
                    };

                    for comment in comments {
                        let comment_id = comment.id.to_string();
                        if !comment_ids.contains(&comment_id) {
                            continue;
                        }

                        if comment.created_at > last_check {
                            let msg = IncomingMessage {
                                channel: "github".to_string(),
                                id: comment_id,
                                thread_id: issue.number.to_string(),
                                author: comment.user.login,
                                body: comment.body,
                                timestamp: comment.created_at,
                                metadata: serde_json::json!({ "issue_number": issue.number }),
                            };

                            if tx.send(msg).await.is_err() {
                                tracing::debug!("github channel receiver dropped");
                                return;
                            }
                        }
                    }
                }

                last_check = Utc::now();
            }
        });

        Ok(rx)
    }

    async fn send(&self, msg: &OutgoingMessage) -> anyhow::Result<()> {
        let issue_number: u64 = msg
            .thread_id
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid issue number: {}", msg.thread_id))?;

        self.post_comment(issue_number, &msg.body)
    }

    async fn stream_output(
        &self,
        thread_id: &str,
        mut rx: broadcast::Receiver<OutputChunk>,
    ) -> anyhow::Result<()> {
        let issue_number: u64 = thread_id
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid issue number: {}", thread_id))?;

        let mut buffer = String::new();
        let mut last_post = std::time::Instant::now();
        let post_interval = std::time::Duration::from_secs(30);

        loop {
            tokio::select! {
                chunk = rx.recv() => {
                    match chunk {
                        Ok(chunk) => {
                            if chunk.is_final {
                                if !buffer.is_empty() {
                                    let _ = self.post_comment(issue_number, &buffer);
                                    buffer.clear();
                                }
                                let _ = self.post_comment(issue_number, "---");
                                let _ = self.post_comment(issue_number, "Session complete.");
                                break;
                            }

                            buffer.push_str(&chunk.content);
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // Skip lagged messages
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                    // Periodic flush
                }
            }

            if last_post.elapsed() >= post_interval && !buffer.is_empty() {
                let _ = self.post_comment(issue_number, &buffer);
                buffer.clear();
                last_post = std::time::Instant::now();
            }
        }

        Ok(())
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        let output = Command::new("gh").args(["auth", "status"]).output()?;

        if !output.status.success() {
            anyhow::bail!("gh auth not logged in");
        }

        tracing::info!("github channel health check passed");
        Ok(())
    }

    async fn shutdown(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
struct WebhookState {
    secret: String,
    repo: String,
    tx: tokio::sync::mpsc::Sender<IncomingMessage>,
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
    body: String,
    user: GhUser,
    created_at: Option<DateTime<Utc>>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct ReviewPayload {
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
    expected == signature
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
                        id: format!("comment-{}-{}", issue.number, timestamp.timestamp()),
                        thread_id: issue.number.to_string(),
                        author: comment.user.login.clone(),
                        body: comment.body.clone(),
                        timestamp: comment.created_at.unwrap_or(timestamp),
                        metadata: serde_json::json!({
                            "event": "issue_comment",
                            "action": action,
                            "issue_number": issue.number
                        }),
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
                        id: format!("review-{}-{}", pr.number, timestamp.timestamp()),
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
    raw_body: axum::extract::Json<WebhookPayload>,
) -> impl IntoResponse {
    let payload = raw_body.0;
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let body_bytes = serde_json::to_vec(&payload).unwrap_or_default();
    if !state.secret.is_empty() && !verify_signature(&state.secret, &body_bytes, signature) {
        tracing::warn!("webhook signature verification failed");
        return (StatusCode::UNAUTHORIZED, "Invalid signature");
    }

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

async fn webhook_health() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

/// Check if the webhook server is healthy by pinging its local health endpoint.
///
/// This only verifies the local HTTP listener is running. It does NOT verify
/// that GitHub can reach the endpoint (NAT/firewall) or that the webhook
/// secret is valid.
pub async fn check_webhook_health(port: u16) -> bool {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_default();
    let url = format!("http://localhost:{}/health", port);
    match client.get(&url).send().await {
        Ok(response) => response.status().is_success(),
        Err(_) => false,
    }
}

pub async fn start_webhook_server(
    port: u16,
    secret: String,
    repo: String,
    tx: tokio::sync::mpsc::Sender<IncomingMessage>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;

    let state = WebhookState { secret, repo, tx };

    let app = Router::new()
        .route("/health", get(webhook_health))
        .route("/webhook", post(handle_webhook))
        .with_state(state);

    tracing::info!(port = port, "starting webhook server");

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
                "body": "@orchestrator please fix this",
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
        assert!(msg.body.contains("@orchestrator"));
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
}
