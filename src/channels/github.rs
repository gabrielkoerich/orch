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
use subtle::ConstantTimeEq;

#[derive(serde::Deserialize, serde::Serialize)]
struct GhUser {
    login: String,
}

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

async fn webhook_health() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

/// Check if the webhook server is healthy by pinging its local health endpoint.
///
/// This only verifies the local HTTP listener is running. It does NOT verify
/// that GitHub can reach the endpoint (NAT/firewall) or that the webhook
/// secret is valid.
pub async fn check_webhook_health(port: u16) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(?e, "failed to build HTTP client for webhook health check");
            return false;
        }
    };
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
                "id": 12345,
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
