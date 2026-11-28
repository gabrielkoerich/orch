//! Mention and owner slash command scanning

use super::{kv_get_prefer_store, kv_set_prefer_store};
use crate::backends::{ExternalBackend, Mention};
use crate::engine::tasks::TaskManager;
use crate::engine::tasks::commands::CommandGh;
use crate::engine::tasks::commands::OwnerCommand;
use crate::engine::tasks::commands::{
    CommandOutcome, CommandStoreOps, parse_command, validate_and_run_command,
};
use crate::github::http::GhHttp;
use crate::github::types::extract_issue_number_from_url;
use crate::store::TaskStore;
use futures::future;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::OnceCell;

pub(super) async fn fetch_comments_since(
    backend: &Arc<dyn ExternalBackend>,
    since: &str,
) -> anyhow::Result<Vec<Mention>> {
    backend.get_mentions(since).await
}

pub(super) fn filter_mentions_by_since(comments: &[Mention], since: &str) -> Vec<Mention> {
    comments
        .iter()
        .filter(|comment| comment.created_at.as_str() > since)
        .cloned()
        .collect()
}

/// Resolve the parent task ID for a mention.
///
/// - If the mention is on a PR, look up the task that owns that PR via `pr_number`.
/// - If the mention is on an issue, look up the task by external_id (issue number).
///
/// Returns:
/// - `Ok(Some(id))` — parent found
/// - `Ok(None)` — no matching parent (genuine miss, e.g. unknown issue/PR)
/// - `Err(_)` — lookup failed (DB error); caller should defer/retry
async fn resolve_mention_parent_id(
    store: &TaskStore,
    repo: &str,
    issue_num: &str,
    is_pr: bool,
) -> anyhow::Result<Option<i64>> {
    if is_pr {
        if let Ok(pr_num) = issue_num.parse::<i32>() {
            store.resolve_id_by_pr_number(repo, pr_num).await
        } else {
            Ok(None)
        }
    } else {
        store.resolve_id_by_external(repo, issue_num).await
    }
}

/// Create a sentinel mention task in the store and advance the cursor.
async fn record_mention_task(
    store: &TaskStore,
    repo: &str,
    mention: &Mention,
    title: &str,
    body: &str,
    parent_id: Option<i64>,
    cursor: &mut MentionCursor,
) -> bool {
    match store
        .create_internal(repo, title, body, "mention", &mention.id, parent_id)
        .await
    {
        Ok(task_id) => {
            tracing::info!(task_id, mention_id = %mention.id, "created mention task");
            cursor.advance(&mention.created_at);
            true
        }
        Err(e) => {
            tracing::warn!(mention_id = %mention.id, err = %e, "failed to create mention task");
            false
        }
    }
}

/// Advance the mention cursor if the timestamp is newer.
fn advance_cursor(last_safe_ts: &mut Option<String>, ts: &str) {
    if last_safe_ts.as_deref() < Some(ts) {
        *last_safe_ts = Some(ts.to_string());
    }
}

#[derive(Debug, Default)]
pub(super) struct MentionCursor {
    pub(super) last_safe_ts: Option<String>,
    blocked_by_gap: bool,
}

impl MentionCursor {
    pub(super) fn advance(&mut self, ts: &str) {
        if self.blocked_by_gap {
            return;
        }
        advance_cursor(&mut self.last_safe_ts, ts);
    }

    pub(super) fn block_on_gap(&mut self) {
        self.blocked_by_gap = true;
    }

    pub(super) fn into_last_safe_ts(self) -> Option<String> {
        self.last_safe_ts
    }
}

/// What to do with a comment in the unified scan loop.
#[derive(Debug, PartialEq)]
pub(crate) enum CommentAction {
    /// Already processed or not relevant — skip.
    Skip,
    /// Slash command without @mention — execute the command.
    ExecuteCommand {
        command: OwnerCommand,
        issue_num: String,
    },
    /// @mention with a slash command — execute and record sentinel task.
    ExecuteCommandForMention {
        command: OwnerCommand,
        issue_num: String,
    },
    /// @mention without command — create internal task for agent to respond.
    CreateMentionTask { issue_num: Option<String> },
}

/// Classify a comment into the appropriate action.
///
/// Pure function — no I/O, no async. Determines what the scan loop should do
/// with each comment based on its content and whether it was already processed.
pub(crate) fn classify_comment(
    body: &str,
    issue_url: Option<&str>,
    current_user: &str,
    already_processed: bool,
) -> CommentAction {
    if already_processed {
        return CommentAction::Skip;
    }

    let is_mention =
        body.contains(current_user) || body.contains("@orch") || body.contains("@orchestrator");
    let command = parse_command(body);
    let issue_num = issue_url.and_then(extract_issue_number_from_url);

    match (command, is_mention, issue_num) {
        (Some(cmd), true, Some(num)) => CommentAction::ExecuteCommandForMention {
            command: cmd,
            issue_num: num,
        },
        (Some(cmd), false, Some(num)) => CommentAction::ExecuteCommand {
            command: cmd,
            issue_num: num,
        },
        // Command but no issue URL — if it's a mention, create task; otherwise skip
        (Some(_), true, None) => CommentAction::CreateMentionTask { issue_num: None },
        // Mention without command — create task for agent
        (None, true, issue_num) => CommentAction::CreateMentionTask { issue_num },
        // Neither mention nor command — skip
        (Some(_), false, None) | (None, false, _) => CommentAction::Skip,
    }
}

/// Handle a slash command from a mention. Records a sentinel task and advances
/// the cursor on success; logs a warning and returns early if the mention has
/// no valid issue URL.
///
/// Execute a slash command mentioned by `@orch` on a comment.
///
/// Returns:
/// - `Ok(true)` — command processed and cursor may continue advancing
/// - `Ok(false)` — mention task insert failed, or validation hit a transient
///   GitHub error; caller must stop cursor advancement so the next tick retries
/// - `Err(_)` — DB lookup failed for parent_id; caller must NOT advance cursor
///   so this mention is retried on the next sync
#[expect(
    clippy::too_many_arguments,
    reason = "the sync tick hands over its shared state piece by piece"
)]
pub(super) async fn handle_slash_command(
    backend: &Arc<dyn ExternalBackend>,
    store: Option<&Arc<TaskStore>>,
    repo: &str,
    task_manager: &Arc<TaskManager>,
    gh: &dyn CommandGh,
    mention: &Mention,
    command: &OwnerCommand,
    cursor: &mut MentionCursor,
) -> anyhow::Result<bool> {
    let Some(issue_num) = mention
        .issue_url
        .as_deref()
        .and_then(extract_issue_number_from_url)
    else {
        tracing::warn!(comment_id = %mention.id, "slash command without valid issue number");
        return Ok(true);
    };

    let store_opt: Option<Arc<dyn CommandStoreOps>> =
        store.map(|s| Arc::clone(s) as Arc<dyn CommandStoreOps>);
    let outcome = validate_and_run_command(
        backend,
        gh,
        repo,
        &issue_num,
        command,
        &mention.author,
        &store_opt,
        task_manager,
    )
    .await;

    // Extract is_pr from the outcome (avoids a second get_issue call)
    let is_pr = match &outcome {
        CommandOutcome::Executed { is_pr }
        | CommandOutcome::NotOpen { is_pr }
        | CommandOutcome::NotCollaborator { is_pr } => *is_pr,
        CommandOutcome::FetchFailed | CommandOutcome::CollaboratorCheckFailed => {
            // Nothing was executed and nothing was posted to GitHub. Leave the cursor
            // before this comment so the next tick retries the command.
            tracing::warn!(
                comment_id = %mention.id,
                command = %command,
                "deferring @orch slash command after a transient GitHub error"
            );
            return Ok(false);
        }
    };

    // Record a sentinel task for the mention (with correct parent_id).
    // On DB lookup failure, defer so the mention can be retried on next sync.
    if let Some(s) = store {
        let parent_id = match resolve_mention_parent_id(s, repo, &issue_num, is_pr).await {
            Ok(parent_id) => parent_id,
            Err(e) => {
                tracing::warn!(
                    comment_id = %mention.id,
                    err = %e,
                    "deferring mention task due to parent lookup failure"
                );
                return Err(e);
            }
        };

        let (title, body) = match &outcome {
            CommandOutcome::NotOpen { .. } => (
                format!(
                    "Skipped @orch command on closed #{issue_num} from @{}",
                    mention.author
                ),
                format!(
                    "Mention by @{}:\n\n{}\n\n**Skipped:** target is closed",
                    mention.author, mention.body
                ),
            ),
            CommandOutcome::NotCollaborator { .. } => (
                format!(
                    "Skipped @orch command on #{issue_num} from non-collaborator @{}",
                    mention.author
                ),
                format!(
                    "Mention by @{}:\n\n{}\n\n**Skipped:** author is not a collaborator",
                    mention.author, mention.body
                ),
            ),
            CommandOutcome::Executed { .. } => (
                format!(
                    "Respond to mention by @{} on task #{issue_num} — command executed",
                    mention.author
                ),
                format!(
                    "Mention by @{} on #{issue_num}:\n\n{}\n\n**Status:** Command executed",
                    mention.author, mention.body
                ),
            ),
            CommandOutcome::FetchFailed | CommandOutcome::CollaboratorCheckFailed => {
                tracing::debug!("skipping mention task record due to earlier command failure");
                return Ok(true);
            }
        };

        let created = record_mention_task(s, repo, mention, &title, &body, parent_id, cursor).await;
        if !created {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Scan for @mentions and handle them.
///
/// Checks recent issue comments for @orch (and legacy @orchestrator) mentions.
/// Unified comment scanner — handles both @mentions and slash commands in one pass.
///
/// For each comment, classifies it via [`classify_comment`] and dispatches:
/// - Slash commands → validate + execute (with sentinel task if it was an @mention)
/// - @mentions without commands → create internal task for agent to respond
/// - Everything else → skip
pub(super) async fn scan_comments(
    backend: &Arc<dyn ExternalBackend>,
    store: Option<&Arc<TaskStore>>,
    repo: &str,
    task_manager: &Arc<TaskManager>,
    prefetched_comments: Option<&[Mention]>,
) -> anyhow::Result<()> {
    let current_user = match backend.get_authenticated_user().await {
        Ok(Some(u)) => format!("@{u}"),
        Ok(None) => {
            tracing::debug!("backend does not support user identity, skipping comment scan");
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(err = %e, "failed to get current user, skipping comment scan");
            return Ok(());
        }
    };

    let comments = if let Some(c) = prefetched_comments {
        c.to_vec()
    } else {
        let fallback = chrono::Utc::now() - chrono::Duration::hours(24);
        let since_str = match kv_get_prefer_store(store, "mentions_last_checked").await {
            Some(ts) => ts,
            None => fallback.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        };
        match backend.get_mentions(&since_str).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(err = %e, "failed to get comments");
                return Ok(());
            }
        }
    };

    // Dedup set: mention tasks already created (by source_id).
    let existing_mentions: HashSet<String> = if let Some(s) = store {
        match s.list_source_ids_by_source(repo, "mention", 30).await {
            Ok(ids) => ids.into_iter().collect(),
            Err(e) => {
                tracing::warn!(err = %e, "failed to load mention source IDs — skipping to prevent duplicates");
                return Ok(());
            }
        }
    } else {
        HashSet::new()
    };

    // Lazily create GhHttp — only allocate when we actually need to make GitHub
    // API calls.  This avoids network initialization in test environments where
    // the code path never reaches an API call.
    let gh_cell: OnceCell<GhHttp> = OnceCell::new();
    let get_gh = || async { gh_cell.get_or_try_init(|| async { GhHttp::new() }).await };

    // Pre-classify all comments up front so we can batch the is_pull_request
    // checks for CreateMentionTask entries before entering the main loop.
    let actions: Vec<CommentAction> = comments
        .iter()
        .map(|c| {
            classify_comment(
                &c.body,
                c.issue_url.as_deref(),
                &current_user,
                existing_mentions.contains(&c.id),
            )
        })
        .collect();

    // Collect (comment_index, issue_num) for all CreateMentionTask entries that
    // need an is_pull_request check, then resolve them all concurrently with
    // join_all to avoid N × RTT serial API calls inside the loop.
    let pr_check_inputs: Vec<(usize, String)> = actions
        .iter()
        .enumerate()
        .filter_map(|(i, action)| {
            if let CommentAction::CreateMentionTask {
                issue_num: Some(num),
            } = action
            {
                Some((i, num.clone()))
            } else {
                None
            }
        })
        .collect();

    // Only create GhHttp if we actually have PR checks to perform.
    let is_pr_values: Vec<bool> = if pr_check_inputs.is_empty() {
        Vec::new()
    } else {
        let gh = get_gh().await?;
        future::join_all(
            pr_check_inputs
                .iter()
                .map(|(_, num)| async move { gh.is_pull_request(repo, num).await }),
        )
        .await
    };

    // Build a lookup map: comment_index → is_pr.
    let is_pr_map: HashMap<usize, bool> = pr_check_inputs
        .iter()
        .map(|(i, _)| *i)
        .zip(is_pr_values)
        .collect();

    let mut cursor = MentionCursor::default();

    for (comment_idx, (comment, action)) in comments.iter().zip(actions).enumerate() {
        match action {
            CommentAction::Skip => {
                cursor.advance(&comment.created_at);
            }

            CommentAction::ExecuteCommand { command, issue_num } => {
                let store_opt: Option<Arc<dyn CommandStoreOps>> =
                    store.map(|s| Arc::clone(s) as Arc<dyn CommandStoreOps>);
                let gh = get_gh().await?;
                let outcome = validate_and_run_command(
                    backend,
                    gh,
                    repo,
                    &issue_num,
                    &command,
                    &comment.author,
                    &store_opt,
                    task_manager,
                )
                .await;
                match outcome {
                    // Validation hit a transient GitHub error: nothing ran and nothing
                    // was posted. Stop the cursor so the next tick retries this comment.
                    CommandOutcome::FetchFailed | CommandOutcome::CollaboratorCheckFailed => {
                        tracing::warn!(
                            comment_id = %comment.id,
                            command = %command,
                            "deferring slash command after a transient GitHub error"
                        );
                        cursor.block_on_gap();
                        break;
                    }
                    // The outcome comment (success or error) was already posted to
                    // GitHub, so re-processing would create duplicates.
                    CommandOutcome::Executed { .. }
                    | CommandOutcome::NotOpen { .. }
                    | CommandOutcome::NotCollaborator { .. } => cursor.advance(&comment.created_at),
                }
            }

            CommentAction::ExecuteCommandForMention { command, .. } => {
                if let Err(e) = backend.acknowledge_mention(&comment.id).await {
                    tracing::debug!(err = %e, mention_id = %comment.id, "failed to acknowledge mention");
                    // Do NOT advance the cursor or call handle_slash_command — the GitHub
                    // reaction was never posted. Retry on next tick once the network
                    // glitch has resolved.
                } else {
                    // Parent lookup failure propagates via `?` — scan_comments returns early
                    // without advancing the cursor so the mention is retried on next sync.
                    let gh = get_gh().await?;
                    let advanced_safely = handle_slash_command(
                        backend,
                        store,
                        repo,
                        task_manager,
                        gh,
                        comment,
                        &command,
                        &mut cursor,
                    )
                    .await?;
                    if !advanced_safely {
                        cursor.block_on_gap();
                        break;
                    }
                }
            }

            CommentAction::CreateMentionTask { issue_num } => {
                if let Err(e) = backend.acknowledge_mention(&comment.id).await {
                    tracing::debug!(err = %e, mention_id = %comment.id, "failed to acknowledge mention");
                    // Do NOT record the mention task or advance the cursor — the GitHub
                    // reaction was never posted. Retry on next tick once the network
                    // glitch has resolved.
                } else {
                    // Use the pre-fetched value — no serial API call here.
                    // issue_num may be None (e.g. bot mentioned with no issue URL), in
                    // which case is_pr_map has no entry; default to false (the None arm
                    // of the downstream match ignores is_pr entirely).
                    let is_pr = is_pr_map.get(&comment_idx).copied().unwrap_or(false);

                    let (title, task_body) = match (&issue_num, is_pr) {
                        (Some(num), false) => (
                            format!("Respond to mention by @{} on task #{num}", comment.author),
                            format!(
                                "Mention by @{} on GitHub issue #{num}:\n\n{}\n\n---\nThis mention was posted on issue #{num}. Review the full issue to understand the context before responding.",
                                comment.author, comment.body
                            ),
                        ),
                        (Some(num), true) => (
                            format!("Respond to mention by @{} on PR #{num}", comment.author),
                            format!(
                                "Mention by @{} on GitHub PR #{num}:\n\n{}\n\n---\nThis mention was posted on PR #{num}. Review the full PR (diff, description, and comments) to understand the context before responding.",
                                comment.author, comment.body
                            ),
                        ),
                        (None, _) => (
                            format!("Respond to mention by @{}", comment.author),
                            format!("Mention by @{}:\n\n{}", comment.author, comment.body),
                        ),
                    };

                    if let Some(s) = store {
                        // On DB lookup failure, do NOT record the mention task or advance
                        // the cursor — the mention will be retried on next sync.
                        let parent_id = match issue_num {
                            Some(ref num) => {
                                match resolve_mention_parent_id(s, repo, num, is_pr).await {
                                    Ok(id) => id,
                                    Err(e) => {
                                        tracing::warn!(
                                            mention_id = %comment.id,
                                            err = %e,
                                            "deferring mention task due to parent lookup failure"
                                        );
                                        return Err(anyhow::anyhow!(
                                            "deferring mention {} due to parent lookup failure",
                                            comment.id
                                        ));
                                    }
                                }
                            }
                            None => None,
                        };
                        if !record_mention_task(
                            s,
                            repo,
                            comment,
                            &title,
                            &task_body,
                            parent_id,
                            &mut cursor,
                        )
                        .await
                        {
                            cursor.block_on_gap();
                            break;
                        }
                    }
                }
            }
        }
    }

    if let Some(ts) = cursor.into_last_safe_ts() {
        kv_set_prefer_store(store, "mentions_last_checked", &ts).await;
    }

    Ok(())
}
