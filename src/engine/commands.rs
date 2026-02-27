//! Owner slash commands — detect and execute /commands in issue comments.
//!
//! Scans recent GitHub issue comments for slash commands (e.g. `/retry`,
//! `/close`, `/block reason`) posted by repo collaborators. Commands are
//! executed against the issue's task and a confirmation comment is posted.

use crate::backends::{ExternalBackend, ExternalId, Status};
use crate::db::Db;
use crate::github::cli::GhCli;
use std::sync::Arc;

/// Parsed owner command from an issue comment.
#[derive(Debug, Clone, PartialEq)]
pub enum OwnerCommand {
    /// Reset to status:new, re-dispatch.
    Retry,
    /// Clear agent, re-route (optionally force agent).
    Reroute(Option<String>),
    /// Mark status:done, close issue.
    Close,
    /// Mark status:blocked with optional reason.
    Block(Option<String>),
    /// Mark status:new, re-dispatch.
    Unblock,
    /// Trigger review agent on current PR.
    Review,
}

impl std::fmt::Display for OwnerCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retry => write!(f, "/retry"),
            Self::Reroute(Some(a)) => write!(f, "/reroute {a}"),
            Self::Reroute(None) => write!(f, "/reroute"),
            Self::Close => write!(f, "/close"),
            Self::Block(Some(r)) => write!(f, "/block {r}"),
            Self::Block(None) => write!(f, "/block"),
            Self::Unblock => write!(f, "/unblock"),
            Self::Review => write!(f, "/review"),
        }
    }
}

/// Parse a slash command from a comment body.
///
/// Scans each line for a `/command` at the start. Returns the first valid
/// command found. Unknown `/something` lines are skipped.
pub fn parse_command(body: &str) -> Option<OwnerCommand> {
    for line in body.lines() {
        let line = line.trim();
        if !line.starts_with('/') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(2, char::is_whitespace).collect();
        let cmd = parts[0];
        let args = parts
            .get(1)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(String::from);

        match cmd {
            "/retry" => return Some(OwnerCommand::Retry),
            "/reroute" => return Some(OwnerCommand::Reroute(args)),
            "/close" => return Some(OwnerCommand::Close),
            "/block" => return Some(OwnerCommand::Block(args)),
            "/unblock" => return Some(OwnerCommand::Unblock),
            "/review" => return Some(OwnerCommand::Review),
            _ => continue,
        }
    }
    None
}

/// Extract issue number from a GitHub API issue URL.
///
/// Expected format: `https://api.github.com/repos/owner/repo/issues/123`
fn extract_issue_number(issue_url: &str) -> Option<String> {
    issue_url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        .map(String::from)
}

/// Scan recent comments for owner slash commands and execute them.
///
/// Uses a timestamp cursor (`owner_commands_last_checked`) for dedup.
/// Reuses the same comment endpoint as `scan_mentions`.
pub async fn scan_commands(
    backend: &Arc<dyn ExternalBackend>,
    db: &Arc<Db>,
    repo: &str,
) -> anyhow::Result<()> {
    let gh = GhCli::new();

    // Use persisted cursor, fall back to 24h ago
    let fallback = chrono::Utc::now() - chrono::Duration::hours(24);
    let since_str = match db.kv_get("owner_commands_last_checked").await {
        Ok(Some(ts)) => ts,
        _ => fallback.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    };

    // Fetch recent comments (same endpoint as mentions)
    let comments = match backend.get_mentions(&since_str).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(err = %e, "failed to fetch comments for command scanning");
            return Ok(());
        }
    };

    if comments.is_empty() {
        // Still advance cursor even if no comments
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        db.kv_set("owner_commands_last_checked", &now).await.ok();
        return Ok(());
    }

    // Build dedup set from already-processed command comment IDs.
    // We store processed IDs in KV to survive restarts within the cursor window.
    let processed_ids: std::collections::HashSet<String> =
        match db.kv_get("owner_commands_processed_ids").await {
            Ok(Some(ids)) if !ids.is_empty() => ids.split(',').map(String::from).collect(),
            _ => std::collections::HashSet::new(),
        };

    let mut new_processed = Vec::new();

    for mention in &comments {
        // Skip already processed
        if processed_ids.contains(&mention.id) {
            continue;
        }

        // Try to parse a command
        let command = match parse_command(&mention.body) {
            Some(cmd) => cmd,
            None => continue,
        };

        // Extract issue number from the comment's issue URL
        let issue_number = match mention.issue_url.as_deref().and_then(extract_issue_number) {
            Some(n) => n,
            None => {
                tracing::warn!(
                    comment_id = %mention.id,
                    "slash command without issue_url, skipping"
                );
                continue;
            }
        };

        // Validate author is repo owner or collaborator
        match gh.is_collaborator(repo, &mention.author).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!(
                    author = %mention.author,
                    command = %command,
                    issue = %issue_number,
                    "ignoring slash command from non-collaborator"
                );
                new_processed.push(mention.id.clone());
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    author = %mention.author,
                    err = %e,
                    "failed to check collaborator status, skipping command"
                );
                continue;
            }
        }

        // Execute the command
        let task_id = ExternalId(issue_number.clone());
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

        let result = execute_command(backend, &gh, repo, &task_id, &command).await;

        match result {
            Ok(msg) => {
                tracing::info!(
                    issue = %issue_number,
                    author = %mention.author,
                    command = %command,
                    "executed owner command"
                );
                let confirmation = format!("[{now}] {msg} — executed by @{}", mention.author);
                if let Err(e) = backend.post_comment(&task_id, &confirmation).await {
                    tracing::warn!(issue = %issue_number, err = %e, "failed to post confirmation");
                }
            }
            Err(e) => {
                tracing::warn!(
                    issue = %issue_number,
                    command = %command,
                    err = %e,
                    "failed to execute owner command"
                );
                let error_msg = format!("[{now}] Failed to execute `{command}`: {e}");
                if let Err(e2) = backend.post_comment(&task_id, &error_msg).await {
                    tracing::warn!(issue = %issue_number, err = %e2, "failed to post error comment");
                }
            }
        }

        new_processed.push(mention.id.clone());
    }

    // Persist processed IDs (keep last 500 to avoid unbounded growth)
    if !new_processed.is_empty() {
        let mut all: Vec<String> = processed_ids.into_iter().collect();
        all.sort();
        all.extend(new_processed);
        if all.len() > 500 {
            all = all.split_off(all.len() - 500);
        }
        db.kv_set("owner_commands_processed_ids", &all.join(","))
            .await
            .ok();
    }

    // Advance cursor
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    if let Err(e) = db.kv_set("owner_commands_last_checked", &now).await {
        tracing::warn!(err = %e, "failed to persist owner commands cursor");
    }

    Ok(())
}

/// Execute a single owner command against a task.
async fn execute_command(
    backend: &Arc<dyn ExternalBackend>,
    gh: &GhCli,
    repo: &str,
    task_id: &ExternalId,
    command: &OwnerCommand,
) -> anyhow::Result<String> {
    match command {
        OwnerCommand::Retry => {
            // Remove agent labels, reset to new
            let task = backend.get_task(task_id).await?;
            for label in &task.labels {
                if label.starts_with("agent:") {
                    backend.remove_label(task_id, label).await.ok();
                }
            }
            backend.update_status(task_id, Status::New).await?;
            Ok("`/retry` — reset to `status:new`, will re-dispatch".to_string())
        }

        OwnerCommand::Reroute(agent) => {
            // Remove existing agent labels
            let task = backend.get_task(task_id).await?;
            for label in &task.labels {
                if label.starts_with("agent:") {
                    backend.remove_label(task_id, label).await.ok();
                }
            }
            // Optionally set new agent
            if let Some(agent_name) = agent {
                let label = format!("agent:{agent_name}");
                backend.set_labels(task_id, &[label]).await?;
            }
            backend.update_status(task_id, Status::New).await?;
            match agent {
                Some(a) => Ok(format!(
                    "`/reroute {a}` — cleared agent, forced `agent:{a}`, reset to `status:new`"
                )),
                None => {
                    Ok("`/reroute` — cleared agent, reset to `status:new`, will re-route".into())
                }
            }
        }

        OwnerCommand::Close => {
            backend.update_status(task_id, Status::Done).await?;
            gh.close_issue(repo, &task_id.0).await?;
            Ok("`/close` — marked `status:done` and closed issue".to_string())
        }

        OwnerCommand::Block(reason) => {
            backend.update_status(task_id, Status::Blocked).await?;
            match reason {
                Some(r) => Ok(format!("`/block` — marked `status:blocked`: {r}")),
                None => Ok("`/block` — marked `status:blocked`".to_string()),
            }
        }

        OwnerCommand::Unblock => {
            backend.update_status(task_id, Status::New).await?;
            Ok("`/unblock` — marked `status:new`, will re-dispatch".to_string())
        }

        OwnerCommand::Review => {
            backend.update_status(task_id, Status::InReview).await?;
            Ok(
                "`/review` — set `status:in_review`, review agent will pick up on next sync"
                    .to_string(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_retry() {
        assert_eq!(parse_command("/retry"), Some(OwnerCommand::Retry));
    }

    #[test]
    fn parse_retry_with_trailing_whitespace() {
        assert_eq!(parse_command("/retry   "), Some(OwnerCommand::Retry));
    }

    #[test]
    fn parse_close() {
        assert_eq!(parse_command("/close"), Some(OwnerCommand::Close));
    }

    #[test]
    fn parse_unblock() {
        assert_eq!(parse_command("/unblock"), Some(OwnerCommand::Unblock));
    }

    #[test]
    fn parse_review() {
        assert_eq!(parse_command("/review"), Some(OwnerCommand::Review));
    }

    #[test]
    fn parse_reroute_no_agent() {
        assert_eq!(parse_command("/reroute"), Some(OwnerCommand::Reroute(None)));
    }

    #[test]
    fn parse_reroute_with_agent() {
        assert_eq!(
            parse_command("/reroute codex"),
            Some(OwnerCommand::Reroute(Some("codex".into())))
        );
    }

    #[test]
    fn parse_block_no_reason() {
        assert_eq!(parse_command("/block"), Some(OwnerCommand::Block(None)));
    }

    #[test]
    fn parse_block_with_reason() {
        assert_eq!(
            parse_command("/block waiting on upstream fix"),
            Some(OwnerCommand::Block(Some("waiting on upstream fix".into())))
        );
    }

    #[test]
    fn parse_ignores_non_command_text() {
        assert_eq!(parse_command("This is a regular comment"), None);
    }

    #[test]
    fn parse_ignores_unknown_commands() {
        assert_eq!(parse_command("/unknown"), None);
    }

    #[test]
    fn parse_command_in_multiline_body() {
        let body = "Some context here\n\n/retry\n\nMore text";
        assert_eq!(parse_command(body), Some(OwnerCommand::Retry));
    }

    #[test]
    fn parse_first_valid_command_wins() {
        let body = "/retry\n/close";
        assert_eq!(parse_command(body), Some(OwnerCommand::Retry));
    }

    #[test]
    fn parse_skips_unknown_finds_valid() {
        let body = "/unknown\n/close";
        assert_eq!(parse_command(body), Some(OwnerCommand::Close));
    }

    #[test]
    fn parse_indented_command() {
        // Commands with leading whitespace should still be detected
        assert_eq!(parse_command("  /retry"), Some(OwnerCommand::Retry));
    }

    #[test]
    fn extract_issue_number_works() {
        assert_eq!(
            extract_issue_number("https://api.github.com/repos/owner/repo/issues/123"),
            Some("123".into())
        );
    }

    #[test]
    fn extract_issue_number_empty_url() {
        assert_eq!(extract_issue_number(""), None);
    }

    #[test]
    fn display_commands() {
        assert_eq!(OwnerCommand::Retry.to_string(), "/retry");
        assert_eq!(
            OwnerCommand::Reroute(Some("codex".into())).to_string(),
            "/reroute codex"
        );
        assert_eq!(OwnerCommand::Reroute(None).to_string(), "/reroute");
        assert_eq!(OwnerCommand::Close.to_string(), "/close");
        assert_eq!(
            OwnerCommand::Block(Some("reason".into())).to_string(),
            "/block reason"
        );
        assert_eq!(OwnerCommand::Block(None).to_string(), "/block");
        assert_eq!(OwnerCommand::Unblock.to_string(), "/unblock");
        assert_eq!(OwnerCommand::Review.to_string(), "/review");
    }
}
