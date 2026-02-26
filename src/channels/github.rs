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

#[derive(serde::Deserialize)]
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
            .args(["api", &url, "--header", "Accept: application/vnd.github.v3+json"])
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
            .args(["api", &url, "--header", "Accept: application/vnd.github.v3+json"])
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

                let issues = match GitHubChannel::new(repo_clone.clone()).fetch_in_progress_issues().await {
                    Ok(i) => i,
                    Err(e) => {
                        tracing::warn!(?e, "failed to fetch in_progress issues");
                        continue;
                    }
                };

                for issue in issues {
                    let comments = match GitHubChannel::new(repo_clone.clone()).fetch_comments(issue.number).await {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(issue = issue.number, ?e, "failed to fetch comments");
                            continue;
                        }
                    };

                    let comment_ids: Vec<String> = {
                        let mut seen_ids = last_comment_ids.lock().unwrap();
                        let ids: Vec<String> = comments.iter()
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
        let output = Command::new("gh")
            .args(["auth", "status"])
            .output()?;

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
