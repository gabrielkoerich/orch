//! Sync tick — periodic operations that run less frequently than the core tick.
//!
//! The sync tick runs every ~45 seconds and handles:
//! - Worktree cleanup for done tasks
//! - Merged-PR detection
//! - @mention scanning and internal task creation
//! - PR review processing and follow-up dispatch
//! - Stale InReview detection
//! - Owner /slash command scanning

use super::EngineConfig;
use super::ci_bootstrap;
use super::cleanup::{check_merged_prs, cleanup_done_worktrees, prune_old_run_output};
use super::commands::{CommandOutcome, CommandStoreOps, parse_command, validate_and_run_command};
use super::review_poll::review_open_prs;
use crate::backends::ExternalId;
use crate::backends::ExternalTask;
use crate::backends::{ExternalBackend, Mention, Status};
use crate::config;
use crate::engine;
use crate::engine::auto_merge::{CI_FAILURE_AUTO_MERGE_STAGE, is_pr_behind, required_checks_state};
use crate::engine::cleanup::cleanup_task_worktree;
use crate::engine::commands::CommandGh;
use crate::engine::commands::OwnerCommand;
use crate::engine::cooldown;
use crate::engine::cooldown::{
    cooldown_reason, github_circuit_remaining_secs, is_agent_in_cooldown, is_github_circuit_open,
    is_model_in_cooldown, refresh_degraded_agents,
};
use crate::engine::dependencies;
use crate::engine::review;
use crate::engine::router::Router;
use crate::engine::router::RouterConfig;
use crate::engine::router::config as router_config;
use crate::engine::runner::agents::patterns;
use crate::engine::tasks;
use crate::engine::tasks::{TaskManager, status_to_task_status};
use crate::github::http::GhHttp;
use crate::github::types::extract_issue_number_from_url;
use crate::store;
use crate::store::Task;
use crate::store::TaskRun;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use crate::tmux::TmuxManager;
use dashmap::{DashMap, DashSet};
use futures::future;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::OnceCell;
use tokio::sync::RwLock;
use tokio::time;

/// Read a KV value from the store.
///
/// Returns `None` if the store is unavailable, the key doesn't exist, OR if
/// the store query fails. Use [`try_kv_get_prefer_store`] if you need to
/// distinguish between "key not found" and actual errors.
async fn kv_get_prefer_store(store: Option<&Arc<TaskStore>>, key: &str) -> Option<String> {
    match try_kv_get_prefer_store(store, key).await {
        Ok(opt) => opt,
        Err(e) => {
            tracing::error!(key, err = %e, "kv_get failed");
            None
        }
    }
}

/// Try to read a KV value from the store, returning errors instead of
/// silently discarding them.
///
/// Returns `Ok(None)` if the store is unavailable or the key doesn't exist.
/// Returns `Err` if the store query fails.
async fn try_kv_get_prefer_store(
    store: Option<&Arc<TaskStore>>,
    key: &str,
) -> anyhow::Result<Option<String>> {
    if let Some(s) = store {
        Ok(s.kv_get(key).await?)
    } else {
        Ok(None)
    }
}

/// Write a KV value to the store.
///
/// Logs warnings on failure but provides no way for callers to know if the
/// write succeeded. Use [`try_kv_set_prefer_store`] if you need to handle
/// errors or guarantee persistence (e.g., for circuit breaker state).
async fn kv_set_prefer_store(store: Option<&Arc<TaskStore>>, key: &str, value: &str) {
    if let Err(e) = try_kv_set_prefer_store(store, key, value).await {
        tracing::error!(key, err = %e, "kv_set failed");
    }
}

/// Try to write a KV value to the store, returning errors instead of
/// silently logging them.
///
/// Returns `Ok(())` if the store is unavailable (best-effort) or if the
/// write succeeds. Returns `Err` if the store query fails.
async fn try_kv_set_prefer_store(
    store: Option<&Arc<TaskStore>>,
    key: &str,
    value: &str,
) -> anyhow::Result<()> {
    if let Some(s) = store {
        s.kv_set(key, value).await?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct ReviewTaskSnapshot {
    pub external: ExternalTask,
    pub stored: Task,
}

async fn prefetch_review_tasks(
    store: &Arc<TaskStore>,
    repo: &str,
) -> anyhow::Result<(
    Vec<ReviewTaskSnapshot>,
    Vec<ReviewTaskSnapshot>,
    Vec<ReviewTaskSnapshot>,
)> {
    // Approved tasks wait on the same merge path as a review in progress
    let mut in_review = store.list_by_status(repo, TaskStatus::InReview).await?;
    in_review.extend(store.list_by_status(repo, TaskStatus::Approved).await?);
    let in_review = in_review
        .into_iter()
        .map(|stored| ReviewTaskSnapshot {
            external: tasks::store_task_to_external(&stored),
            stored,
        })
        .collect();
    let needs_review = store
        .list_by_status(repo, TaskStatus::NeedsReview)
        .await?
        .into_iter()
        .map(|stored| ReviewTaskSnapshot {
            external: tasks::store_task_to_external(&stored),
            stored,
        })
        .collect();

    // Also prefetch blocked tasks that have a branch or PR number recorded.
    // These can be merged externally (or manually) but previously were invisible
    // to the merged-PR checker, leaving tasks stuck in Blocked after merges.
    let blocked_candidates = store
        .list_by_status(repo, TaskStatus::Blocked)
        .await?
        .into_iter()
        .filter(|stored| stored.pr_number.is_some() || !stored.branch.is_empty())
        .map(|stored| ReviewTaskSnapshot {
            external: tasks::store_task_to_external(&stored),
            stored,
        })
        .collect();

    Ok((in_review, needs_review, blocked_candidates))
}

async fn fetch_comments_since(
    backend: &Arc<dyn ExternalBackend>,
    since: &str,
) -> anyhow::Result<Vec<Mention>> {
    backend.get_mentions(since).await
}

fn filter_mentions_by_since(comments: &[Mention], since: &str) -> Vec<Mention> {
    comments
        .iter()
        .filter(|comment| comment.created_at.as_str() > since)
        .cloned()
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureCategory {
    FalseFailure,
    RateLimit,
    CreditExhausted,
    SilentExit0,
    Timeout,
    ConnectionError,
    CliFlagError,
    AllAgentsExhausted,
    ParseError,
    MaxAttempts,
    PushFailed,
    PrCreateFailed,
    ModelUnavailable,
    Unknown,
}

impl FailureCategory {
    /// Whether auto-unblock may put the task back in the queue on its own.
    ///
    /// `MaxAttempts` is deliberately absent: the task already ran the configured
    /// number of times, so an automatic retry just restarts the loop that produced
    /// the block. It stays blocked until `orch task unblock` or `orch task retry`
    /// (#3783)
    fn is_recoverable(self) -> bool {
        matches!(
            self,
            FailureCategory::FalseFailure
                | FailureCategory::RateLimit
                | FailureCategory::CreditExhausted
                | FailureCategory::SilentExit0
                | FailureCategory::Timeout
                | FailureCategory::ConnectionError
                | FailureCategory::CliFlagError
                | FailureCategory::AllAgentsExhausted
                | FailureCategory::ParseError
                | FailureCategory::ModelUnavailable
        )
    }

    fn as_str(self) -> &'static str {
        match self {
            FailureCategory::FalseFailure => "FalseFailure",
            FailureCategory::RateLimit => "RateLimit",
            FailureCategory::CreditExhausted => "CreditExhausted",
            FailureCategory::SilentExit0 => "SilentExit0",
            FailureCategory::Timeout => "Timeout",
            FailureCategory::ConnectionError => "ConnectionError",
            FailureCategory::CliFlagError => "CliFlagError",
            FailureCategory::AllAgentsExhausted => "AllAgentsExhausted",
            FailureCategory::ParseError => "ParseError",
            FailureCategory::MaxAttempts => "MaxAttempts",
            FailureCategory::PushFailed => "PushFailed",
            FailureCategory::PrCreateFailed => "PrCreateFailed",
            FailureCategory::ModelUnavailable => "ModelUnavailable",
            FailureCategory::Unknown => "Unknown",
        }
    }
}

fn classify_failure(error: &str, outcome: &str) -> FailureCategory {
    let lower = error.to_lowercase();

    if outcome == "timeout" || lower.contains("timeout") {
        return FailureCategory::Timeout;
    }

    // Credit exhaustion is a transient condition (credits replenish, billing cycles
    // reset). Classifying it as recoverable allows auto-unblock to retry once the
    // credit-related cooldown expires, instead of permanently blocking the task.
    //
    // Checked before the generic rate-limit branch below: billing-cycle exhaustion
    // messages (e.g. Kimi's "usage limit for this billing cycle") also contain
    // "usage limit"/"rate limit" substrings, so checking outcome/content-based
    // rate-limit patterns first would misclassify them as plain RateLimit (issue
    // #3529). `outcome` also carries the specific string from `classify_run_outcome`
    // when the task_run already recorded it.
    if outcome == "billing_cycle_exhausted"
        || outcome == "credit_exhausted"
        || outcome == "org_level_disabled"
        || cooldown::detect_credit_exhaustion(&lower).is_some()
    {
        return FailureCategory::CreditExhausted;
    }

    if outcome == "rate_limit" || lower.contains("rate limit") || lower.contains("usage limit") {
        return FailureCategory::RateLimit;
    }

    if error.trim().is_empty() {
        return FailureCategory::FalseFailure;
    }

    if lower.contains("all agents exhausted") {
        return FailureCategory::AllAgentsExhausted;
    }

    if lower.contains("max attempts") || lower.contains("exceeded max attempts") {
        return FailureCategory::MaxAttempts;
    }

    if lower.contains("output-format=stream-json") && lower.contains("requires --verbose") {
        return FailureCategory::CliFlagError;
    }

    if lower.contains("model unavailable")
        || lower.contains("model not found")
        || (lower.contains("does not exist") && lower.contains("model"))
    {
        return FailureCategory::ModelUnavailable;
    }

    if lower.contains("push failed") {
        return FailureCategory::PushFailed;
    }

    if lower.contains("create pr failed")
        || lower.contains("failed to create pull request")
        || lower.contains("pull request creation failed")
    {
        return FailureCategory::PrCreateFailed;
    }

    if lower.contains("exit 0") {
        return FailureCategory::SilentExit0;
    }

    if outcome == "parse_error"
        || outcome == "truncated"
        || lower.contains("invalid response")
        || lower.contains("parse error")
        || lower.contains("truncated")
    {
        return FailureCategory::ParseError;
    }

    if patterns::detect_network_error(&lower).is_some() {
        return FailureCategory::ConnectionError;
    }

    FailureCategory::Unknown
}

fn classify_failure_from_run(run: &TaskRun) -> Option<FailureCategory> {
    // Completed successfully — no failure to classify.
    if run.outcome == "success" && run.error.trim().is_empty() {
        return None;
    }

    // Skip runs with no meaningful data. Some runs are created then never
    // populated (aborted/stale work). Treat these as non-failures so they
    // don't block auto-unblock logic which considers recent failures.
    // Historically an all-NULL/empty run would be promoted to Unknown which
    // prevented auto-unblock. Ensure we early-return None for truly empty
    // runs (no outcome, no error, no exit_code, no output).
    if run.outcome.trim().is_empty()
        && run.error.trim().is_empty()
        && run.exit_code.is_none()
        && run.stdout.trim().is_empty()
        && run.stderr.trim().is_empty()
    {
        return None;
    }

    let has_output = !run.stdout.trim().is_empty() || !run.stderr.trim().is_empty();
    if run.exit_code == Some(0) && !has_output && run.outcome != "success" {
        return Some(FailureCategory::SilentExit0);
    }

    let mut category = classify_failure(&run.error, &run.outcome);

    // Promote a bare "false failure" to Unknown only when we have a
    // concrete non-zero exit code or other signs the run truly failed.
    // Do NOT promote when exit_code is None (incomplete run) — those are
    // handled above by skipping. This prevents NULL/aborted runs from
    // turning recoverable FalseFailure into non-recoverable Unknown.
    // Only promote when we have explicit evidence: a non-zero exit code
    // or non-empty stdout/stderr. If exit_code is None we keep FalseFailure
    // so the run remains recoverable.
    if category == FailureCategory::FalseFailure {
        let has_nonzero_exit = run.exit_code.is_some() && run.exit_code != Some(0);
        let has_output = !run.stdout.trim().is_empty() || !run.stderr.trim().is_empty();
        if has_nonzero_exit || has_output {
            category = FailureCategory::Unknown;
        }
    }

    Some(category)
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
struct MentionCursor {
    last_safe_ts: Option<String>,
    blocked_by_gap: bool,
}

impl MentionCursor {
    fn advance(&mut self, ts: &str) {
        if self.blocked_by_gap {
            return;
        }
        advance_cursor(&mut self.last_safe_ts, ts);
    }

    fn block_on_gap(&mut self) {
        self.blocked_by_gap = true;
    }

    fn into_last_safe_ts(self) -> Option<String> {
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
async fn handle_slash_command(
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

fn auto_unblock_cooldown_elapsed(count: i32, last_at: &str) -> bool {
    if count == 0 {
        return true;
    }

    let required = match count {
        1 => chrono::Duration::minutes(30),
        2 => chrono::Duration::hours(2),
        _ => return false,
    };

    if last_at.trim().is_empty() {
        return true;
    }

    match chrono::DateTime::parse_from_rfc3339(last_at) {
        Ok(ts) => chrono::Utc::now() - ts.with_timezone(&chrono::Utc) >= required,
        Err(_) => true,
    }
}

/// True when a blocked task reached auto-merge and CI failed there.
/// Pre-review and bootstrap CI-failure reasons share the "CI failure limit" words
/// and must stay blocked for a person (#4140)
fn is_ci_failure_block(task: &Task) -> bool {
    let reason = task.block_reason.as_deref().unwrap_or("");
    if reason.contains("CI checks timed out") {
        return true;
    }
    reason.contains("CI failure limit") && reason.contains(CI_FAILURE_AUTO_MERGE_STAGE)
}

fn ci_failure_unblock_cooldown_elapsed(task: &Task) -> bool {
    if task.auto_unblock_count == 0 {
        return true;
    }

    // Short cooldowns — the PR may close quickly after the first check.
    // We never permanently block (no `_ => return false`): eventually the PR
    // will close or a human will intervene.
    let required = match task.auto_unblock_count {
        1 => chrono::Duration::minutes(10),
        2 => chrono::Duration::hours(1),
        3 => chrono::Duration::hours(6),
        _ => chrono::Duration::hours(24),
    };

    let last_at = &task.auto_unblock_last_at;
    if last_at.trim().is_empty() {
        return true;
    }

    match chrono::DateTime::parse_from_rfc3339(last_at) {
        Ok(ts) => chrono::Utc::now() - ts.with_timezone(&chrono::Utc) >= required,
        Err(_) => true,
    }
}

async fn try_unblock_ci_failure_task(
    gh: &GhHttp,
    repo: &str,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    task: &Task,
    auto_merge_in_flight: &Arc<DashSet<String>>,
) -> anyhow::Result<bool> {
    let Some(pr_number) = task.pr_number.and_then(|n| u64::try_from(n).ok()) else {
        tracing::debug!(
            task_id = task.id,
            "CI failure blocked task has no PR number"
        );
        return Ok(false);
    };

    let pr = match gh.get_pr(repo, pr_number).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(task_id = task.id, pr_number, err = %e, "failed to get PR status for CI failure block");
            return Ok(false);
        }
    };

    tracing::debug!(
        task_id = task.id,
        pr_number,
        merged = pr.merged,
        state = ?pr.state,
        "verifying PR state for CI failure blocked task"
    );

    if !pr.merged.unwrap_or(false) && pr.state.eq_ignore_ascii_case("closed") {
        tracing::info!(
            task_id = task.id,
            pr_number,
            "PR closed unmerged — leaving CI failure blocked task blocked"
        );
        let new_reason = format!(
            "CI failure limit reached (PR #{pr_number} closed unmerged — no further auto-recovery)"
        );
        if task.block_reason.as_deref() != Some(new_reason.as_str()) {
            let fields = vec![("block_reason", serde_json::json!(new_reason))];
            store.set_fields_silent(task.id, &fields).await?;
        }
        return Ok(false);
    }

    if pr.merged.unwrap_or(false) {
        tracing::info!(
            task_id = task.id,
            pr_number,
            "PR merged — unblocking CI failure blocked task"
        );
        let ext_id = task
            .external_id
            .clone()
            .unwrap_or_else(|| format!("internal:{}", task.id));
        // Update the store directly by the already-resolved numeric id instead of going
        // through `task_manager.update_task_status`, which resolves by `self.repo` — this
        // sweep runs cross-repo, so `task.repo` may not match the caller's own repo scope.
        task_manager
            .mark_task_done_by_store_id(task.id, &task.repo, &ExternalId(ext_id))
            .await?;
        let fields = vec![
            ("block_reason", serde_json::Value::Null),
            ("auto_unblock_count", serde_json::json!(0)),
            ("auto_unblock_last_at", serde_json::json!("")),
            ("auto_unblock_last_reason", serde_json::json!("")),
        ];
        store.set_fields(task.id, &fields).await?;
        return Ok(true);
    }

    // Skip if a recovery merge is already in flight for this task. The poll loop
    // can take up to 10 minutes, and the same task may be seen by both the per-repo
    // sync_tick and the global sweep in the same tick window.
    let task_id_str = task
        .external_id
        .clone()
        .unwrap_or_else(|| format!("internal:{}", task.id));
    if auto_merge_in_flight.contains(&task_id_str) {
        tracing::debug!(
            task_id = task.id,
            pr_number,
            "CI-failure recovery merge already in flight — skipping"
        );
        return Ok(false);
    }

    // Recovery was built for approved PRs. A pre-review CI block can share a
    // rewritten "during auto-merge" reason, so require a recorded approval
    if !review::has_recorded_approval(store, repo, &task_id_str, pr_number).await {
        tracing::info!(
            task_id = task.id,
            pr_number,
            "CI-failure recovery skipped, PR has no approving review"
        );
        return Ok(false);
    }

    // Give the PR an active chance to recover: merge the base branch into the PR
    // branch to re-trigger CI, then re-enable GitHub auto-merge. GitHub's queue
    // can silently stall even when checks are green and the branch is up to date,
    // so after arming auto-merge we poll for completion and fall back to a direct
    // merge once the PR is mergeable and CI is passing.
    let branch_update_result = gh.update_pr_branch(repo, pr_number).await;
    match branch_update_result {
        Ok(()) => {
            tracing::info!(
                task_id = task.id,
                pr_number,
                "updated CI-failure-blocked PR branch against base to retrigger CI"
            );
        }
        Err(ref e) => {
            // warn, not debug, so a silently-broken corrective retry doesn't look like a working one (#3561)
            tracing::warn!(
                task_id = task.id,
                pr_number,
                err = %e,
                outcome = "branch_update_failed",
                "failed to update CI-failure-blocked PR branch (may already be up to date)"
            );
        }
    }

    if let Err(e) = gh.enable_auto_merge(repo, pr_number).await {
        tracing::warn!(task_id = task.id, pr_number, err = %e, "failed to re-enable auto-merge after branch update");
    }

    // Record this attempt so the cooldown advances even though the actual
    // poll/merge now runs in the background.
    let pr_state = pr.state.as_str();
    let current_reason = task.block_reason.as_deref().unwrap_or("");
    let rewrite_reason = current_reason.contains(CI_FAILURE_AUTO_MERGE_STAGE);
    let new_reason = if let Err(ref e) = branch_update_result {
        format!(
            "CI failure limit reached {CI_FAILURE_AUTO_MERGE_STAGE} (PR #{pr_number} still open, state: {pr_state}, branch update failed: {e})"
        )
    } else {
        format!(
            "CI failure limit reached {CI_FAILURE_AUTO_MERGE_STAGE} (PR #{pr_number} still open, state: {pr_state})"
        )
    };
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    // Increment auto_unblock_count so the cooldown backoff (24h, 3d, never) kicks in
    // and we don't hammer the GitHub API every tick for a PR that stays open.
    let new_count = task.auto_unblock_count.saturating_add(1);
    let mut fields = Vec::new();
    if rewrite_reason {
        fields.push(("block_reason", serde_json::json!(&new_reason)));
    }
    fields.push(("auto_unblock_count", serde_json::json!(new_count)));
    fields.push(("auto_unblock_last_at", serde_json::json!(&now)));
    // Use set_fields_silent so the probe write does not bump updated_at on stale
    // blocked tasks — triage queries keyed off updated_at must only see real
    // status/result changes, not periodic re-checks of an open PR.
    store.set_fields_silent(task.id, &fields).await?;

    // Spawn the poll and direct-merge fallback in the background so the
    // recovery sweep does not block the main tick loop (or sync_tick) while
    // CI runs. The in-flight guard prevents double-spawns across ticks.
    auto_merge_in_flight.insert(task_id_str.clone());
    let gh_clone = gh.clone();
    let repo_string = repo.to_string();
    let task_manager_clone = Arc::clone(task_manager);
    let store_clone = Arc::clone(store);
    let task_clone = task.clone();
    let in_flight_clone = Arc::clone(auto_merge_in_flight);

    tokio::spawn(async move {
        let _guard = scopeguard::guard((), |()| {
            in_flight_clone.remove(&task_id_str);
        });

        match poll_and_merge_recovered_pr(&gh_clone, &repo_string, pr_number, task_clone.id).await {
            Ok(true) => {
                tracing::info!(
                    task_id = task_clone.id,
                    pr_number,
                    "CI-failure-blocked PR merged during recovery sweep"
                );
                let ext_id = task_clone
                    .external_id
                    .clone()
                    .unwrap_or_else(|| format!("internal:{}", task_clone.id));
                let fields = [
                    ("block_reason", serde_json::Value::Null),
                    ("auto_unblock_count", serde_json::json!(0)),
                    ("auto_unblock_last_at", serde_json::json!("")),
                    ("auto_unblock_last_reason", serde_json::json!("")),
                ];
                if let Err(e) = task_manager_clone
                    .update_task_status_and_result_by_store_id(
                        task_clone.id,
                        &task_clone.repo,
                        &ExternalId(ext_id),
                        Status::Done,
                        &fields,
                    )
                    .await
                {
                    tracing::warn!(
                        task_id = task_clone.id,
                        pr_number,
                        err = %e,
                        "failed to mark CI-failure-blocked task done after recovery merge"
                    );
                }
                if let Err(e) =
                    cleanup_task_worktree(&format!("{}", task_clone.id), &repo_string, &store_clone)
                        .await
                {
                    tracing::warn!(
                        task_id = task_clone.id,
                        pr_number,
                        err = %e,
                        "cleanup after CI-failure recovery merge failed"
                    );
                }
            }
            Ok(false) => {
                tracing::debug!(
                    task_id = task_clone.id,
                    pr_number,
                    "CI-failure-blocked PR not merged this recovery tick — poll completed without merge"
                );
            }
            Err(e) => {
                tracing::warn!(
                    task_id = task_clone.id,
                    pr_number,
                    err = %e,
                    "error while polling CI-failure-blocked PR for recovery"
                );
            }
        }
    });

    Ok(false)
}

/// Poll a recovered CI-failure PR until it merges or its checks settle, then
/// attempt a direct merge if GitHub's auto-merge queue has not completed it.
///
/// GitHub's `enablePullRequestAutoMerge` hands the PR off to GitHub's queue and
/// returns immediately; the queue can stall silently. This helper mirrors the
/// primary `auto_merge_pr` polling pattern but is scoped to the recovery sweep:
/// it waits for required checks to pass, verifies the PR is not behind the base
/// branch, and calls `gh.merge_pr()` directly when the PR is ready but still
/// open. Returns `Ok(true)` only when the PR is merged, `Ok(false)` when the
/// PR is closed unmerged or needs to wait for a future tick.
async fn poll_and_merge_recovered_pr(
    gh: &GhHttp,
    repo: &str,
    pr_number: u64,
    task_id: i64,
) -> anyhow::Result<bool> {
    // The branch can fall behind while CI runs. Retry the update-branch call a
    // few times inside the poll loop instead of exiting and waiting for the next
    // 24-hour recovery cooldown.
    const MAX_BEHIND_RETRIES: u32 = 3;

    let max_wait_secs: u64 = config::get("workflow.ci_poll_max_wait_secs")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600);
    let base_interval_secs: u64 = config::get("workflow.ci_poll_interval_secs")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let max_wait = Duration::from_secs(max_wait_secs);
    let start = Instant::now();
    let mut poll_count: u32 = 0;
    let mut behind_retries: u32 = 0;

    let initial_pr = gh.get_pr(repo, pr_number).await?;
    let base_branch = initial_pr.base.ref_.clone();
    let required_contexts = gh
        .get_required_status_check_contexts(repo, &base_branch)
        .await?;
    let repo_has_workflows = if required_contexts.is_empty() {
        gh.has_workflows(repo).await?
    } else {
        true
    };

    loop {
        let pr = gh.get_pr(repo, pr_number).await?;
        if pr.merged.unwrap_or(false) {
            tracing::info!(
                task_id = task_id,
                pr_number,
                "PR merged during recovery poll"
            );
            return Ok(true);
        }
        if pr.state.eq_ignore_ascii_case("closed") {
            tracing::info!(
                task_id = task_id,
                pr_number,
                "PR closed unmerged during recovery poll — leaving task blocked"
            );
            return Ok(false);
        }

        if pr.mergeable == Some(false) {
            tracing::info!(
                task_id = task_id,
                pr_number,
                outcome = "poll_exited_without_merge",
                reason = "merge_conflicts",
                "PR has merge conflicts after recovery update — leaving for next recovery cycle"
            );
            return Ok(false);
        }

        if is_pr_behind(&pr) {
            if behind_retries >= MAX_BEHIND_RETRIES {
                tracing::info!(
                    task_id = task_id,
                    pr_number,
                    behind_retries,
                    max_behind_retries = MAX_BEHIND_RETRIES,
                    outcome = "poll_exited_without_merge",
                    reason = "behind_retry_exhausted",
                    "PR still behind base after recovery retries — leaving for next recovery cycle"
                );
                return Ok(false);
            }
            behind_retries += 1;
            match gh.update_pr_branch(repo, pr_number).await {
                Ok(()) => {
                    tracing::info!(
                        task_id = task_id,
                        pr_number,
                        behind_retries,
                        "PR was behind base — updated branch and continuing recovery poll"
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = task_id,
                        pr_number,
                        behind_retries,
                        err = %e,
                        outcome = "branch_update_failed",
                        "failed to update behind PR branch during recovery poll"
                    );
                    return Ok(false);
                }
            }
        }

        let head_sha = pr.head.sha.clone();
        let (state, total, passing, failing, pending) = if required_contexts.is_empty() {
            gh.get_combined_status(
                repo,
                &head_sha,
                repo_has_workflows,
                pr.mergeable_state.as_deref(),
            )
            .await?
        } else {
            let check_runs = gh.get_check_runs(repo, &head_sha).await?;
            let statuses = gh.get_commit_status_contexts(repo, &head_sha).await?;
            required_checks_state(&required_contexts, &check_runs, &statuses)
        };

        tracing::info!(
            task_id = task_id,
            pr_number,
            state = %state,
            total,
            passing,
            failing,
            pending,
            "CI recovery status check"
        );

        match state.as_str() {
            "success" => {
                tracing::info!(
                    task_id = task_id,
                    pr_number,
                    "CI green after recovery — attempting direct merge"
                );
                match gh.merge_pr(repo, pr_number, true, &head_sha).await {
                    Ok(()) => {
                        tracing::info!(
                            task_id = task_id,
                            pr_number,
                            "direct merge succeeded after recovery"
                        );
                        return Ok(true);
                    }
                    Err(e) => {
                        let err = e.to_string().to_ascii_lowercase();
                        tracing::warn!(
                            task_id = task_id,
                            pr_number,
                            err = %e,
                            "direct merge failed after recovery"
                        );
                        // If the PR became merged between the get_pr call and the
                        // merge attempt, treat it as success rather than a failed tick.
                        if err.contains("already merged")
                            || err.contains("no merge commit")
                            || err.contains("pull request already merged")
                        {
                            return Ok(true);
                        }
                        // The head moved after we read it, and updating the branch would skip CI on the new commit
                        if err.contains("head branch was modified")
                            || err.contains("sha does not match")
                        {
                            tracing::info!(
                                task_id = task_id,
                                pr_number,
                                outcome = "poll_exited_without_merge",
                                reason = "head_moved",
                                "merge rejected because the head sha changed"
                            );
                            return Ok(false);
                        }
                        // GitHub may reject the merge because the branch fell behind
                        // again while CI was finishing. Retry the branch update inside
                        // the poll window instead of waiting for the next sweep.
                        if (err.contains("head branch is out of date")
                            || err.contains("behind")
                            || err.contains("409"))
                            && behind_retries < MAX_BEHIND_RETRIES
                        {
                            behind_retries += 1;
                            match gh.update_pr_branch(repo, pr_number).await {
                                Ok(()) => {
                                    tracing::info!(
                                        task_id = task_id,
                                        pr_number,
                                        behind_retries,
                                        "merge rejected due to out-of-date branch — updated and continuing recovery poll"
                                    );
                                    continue;
                                }
                                Err(update_err) => {
                                    tracing::warn!(
                                        task_id = task_id,
                                        pr_number,
                                        behind_retries,
                                        err = %update_err,
                                        outcome = "branch_update_failed",
                                        "failed to update out-of-date PR branch during recovery poll"
                                    );
                                }
                            }
                        }
                        tracing::info!(
                            task_id = task_id,
                            pr_number,
                            outcome = "poll_exited_without_merge",
                            reason = "direct_merge_failed",
                            "direct merge did not succeed — leaving for next recovery cycle"
                        );
                        return Ok(false);
                    }
                }
            }
            "failure" => {
                tracing::info!(
                    task_id = task_id,
                    pr_number,
                    outcome = "poll_exited_without_merge",
                    reason = "ci_failure",
                    "CI still failing after recovery — will retry later"
                );
                return Ok(false);
            }
            _ => {}
        }

        if start.elapsed() >= max_wait {
            tracing::info!(
                task_id = task_id,
                pr_number,
                outcome = "poll_exited_without_merge",
                reason = "poll_timeout",
                "CI still pending after recovery poll timeout — will retry later"
            );
            return Ok(false);
        }

        poll_count += 1;
        let multiplier = (2_u64).pow(poll_count.saturating_sub(1)).min(4);
        let sleep_secs = base_interval_secs.saturating_mul(multiplier);
        time::sleep(Duration::from_secs(sleep_secs)).await;
    }
}

/// Exact `block_reason` written when a task's NeedsReview refire counter exhausts
/// during a rebroadcast escalation (`refire_and_escalate_stale_needs_review_global`).
/// Shared with `auto_recover_rebroadcast_blocked_tasks_global` so escalation and
/// recovery never drift apart on the literal string
const REBROADCAST_BLOCK_REASON: &str = "review agent rebroadcast escalated after repeated retries";

/// Global sweep: auto-recover rebroadcast-blocked tasks across all repos.
///
/// When a NeedsReview refire counter exhausts, the task is blocked with
/// `REBROADCAST_BLOCK_REASON`. Once any review agent is routable again, this
/// sweep resets matching tasks to `NeedsReview` so the subscriber can pick them up.
/// Writes through the store via `*_by_store_id` helpers, not a repo-scoped
/// `TaskManager`. Called once per main tick from `tick_global_sweeps`.
///
/// Before resetting, the attached PR's live state is checked (mirroring
/// `try_unblock_ci_failure_task`): merged means the task is marked done, closed
/// unmerged means it stays blocked with an annotated reason, and only an open PR
/// (or no PR) falls through to `NeedsReview`. A 5-minute block age prevents
/// same-tick escalate/recover flapping. Reset-to-NeedsReview still requires at
/// least one routable review agent
pub(crate) async fn auto_recover_rebroadcast_blocked_tasks_global(
    router: &Router,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    active_repos: &HashSet<String>,
) -> anyhow::Result<()> {
    const MIN_BLOCK_AGE_MINUTES: i64 = 5;

    let all_blocked = match store.list_all_by_status_global(TaskStatus::Blocked).await {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(err = %e, "global rebroadcast-blocked sweep: failed to list blocked tasks");
            return Ok(());
        }
    };

    let now = chrono::Utc::now();
    let candidates: Vec<_> = all_blocked
        .into_iter()
        .filter(|t| {
            if t.block_reason.as_deref() != Some(REBROADCAST_BLOCK_REASON) {
                return false;
            }
            // A repo outside the active set has no review subscriber, so a reset would loop
            if !active_repos.contains(&t.repo) {
                return false;
            }
            // Only recover tasks blocked long enough to rule out same-tick escalations.
            match chrono::DateTime::parse_from_rfc3339(&t.updated_at) {
                Ok(updated_at) => {
                    let age = now - updated_at.with_timezone(&chrono::Utc);
                    age.num_minutes() >= MIN_BLOCK_AGE_MINUTES
                }
                Err(_) => true,
            }
        })
        .collect();

    if candidates.is_empty() {
        return Ok(());
    }

    let gh = match GhHttp::new() {
        Ok(g) => Some(g),
        Err(e) => {
            tracing::warn!(err = %e, "global rebroadcast-blocked sweep: failed to create GhHttp — skipping PR-state checks this tick");
            None
        }
    };

    // Skip reset-to-NeedsReview when no review agent is routable, re-firing
    // while cooled would immediately re-exhaust the counter
    let any_routable =
        !router.available_agents.is_empty() && router.healthy_agent_count("review") > 0;

    for task in candidates {
        let ext_id = task
            .external_id
            .clone()
            .unwrap_or_else(|| format!("internal:{}", task.id));

        if let (Some(gh), Some(pr_number)) =
            (&gh, task.pr_number.and_then(|n| u64::try_from(n).ok()))
        {
            match gh.get_pr(&task.repo, pr_number).await {
                Ok(pr) => {
                    if pr.merged.unwrap_or(false) {
                        tracing::info!(
                            task_id = task.id,
                            repo = %task.repo,
                            pr_number,
                            "global rebroadcast-blocked sweep: PR already merged — marking task done"
                        );
                        if let Err(e) = task_manager
                            .mark_task_done_by_store_id(
                                task.id,
                                &task.repo,
                                &ExternalId(ext_id.clone()),
                            )
                            .await
                        {
                            tracing::warn!(task_id = task.id, repo = %task.repo, err = %e, "global rebroadcast-blocked sweep: failed to mark merged task done");
                            continue;
                        }
                        let fields: &[(&str, serde_json::Value)] = &[
                            ("block_reason", serde_json::Value::Null),
                            ("needs_review_refires", serde_json::json!(0)),
                        ];
                        if let Err(e) = store.set_fields(task.id, fields).await {
                            tracing::warn!(task_id = task.id, err = %e, "global rebroadcast-blocked sweep: failed to clear fields after marking done");
                        }
                        continue;
                    }
                    if pr.state.eq_ignore_ascii_case("closed") {
                        tracing::info!(
                            task_id = task.id,
                            repo = %task.repo,
                            pr_number,
                            "global rebroadcast-blocked sweep: PR closed unmerged — leaving task blocked, not re-dispatching"
                        );
                        let new_reason = format!(
                            "{REBROADCAST_BLOCK_REASON} (PR #{pr_number} closed unmerged — no further auto-recovery)"
                        );
                        let fields: &[(&str, serde_json::Value)] =
                            &[("block_reason", serde_json::json!(new_reason))];
                        // set_fields_silent: this is a terminal annotation, not a real
                        // status change — don't bump updated_at so triage queries keyed
                        // off it aren't confused by a task that stays blocked forever.
                        if let Err(e) = store.set_fields_silent(task.id, fields).await {
                            tracing::warn!(task_id = task.id, err = %e, "global rebroadcast-blocked sweep: failed to annotate closed-unmerged block reason");
                        }
                        continue;
                    }
                    // PR still open — fall through to the reset-to-NeedsReview path.
                }
                Err(e) => {
                    tracing::warn!(task_id = task.id, repo = %task.repo, pr_number, err = %e, "global rebroadcast-blocked sweep: failed to get PR status — skipping this tick");
                    continue;
                }
            }
        }

        if !any_routable {
            continue;
        }

        if let Err(e) = task_manager
            .update_task_status_by_store_id(
                task.id,
                &task.repo,
                &ExternalId(ext_id.clone()),
                Status::NeedsReview,
            )
            .await
        {
            tracing::warn!(task_id = task.id, repo = %task.repo, err = %e, "global rebroadcast-blocked sweep: failed to reset task to needs_review — skipping");
            continue;
        }

        let fields: &[(&str, serde_json::Value)] = &[
            ("block_reason", serde_json::Value::Null),
            ("needs_review_refires", serde_json::json!(0)),
        ];
        if let Err(e) = store.set_fields(task.id, fields).await {
            tracing::warn!(task_id = task.id, err = %e, "global rebroadcast-blocked sweep: failed to clear block_reason/needs_review_refires after status reset");
        }

        tracing::info!(
            task_id = task.id,
            repo = %task.repo,
            ext_id = %ext_id,
            "global rebroadcast-blocked sweep: auto-recovered rebroadcast-blocked task — review agents now available"
        );
    }

    Ok(())
}

async fn auto_unblock_blocked_tasks(
    repo: &str,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    dispatching: &Arc<DashMap<String, String>>,
    auto_merge_in_flight: &Arc<DashSet<String>>,
) -> anyhow::Result<()> {
    let blocked = match store.list_by_status(repo, TaskStatus::Blocked).await {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(err = %e, "failed to list blocked tasks — skipping auto-unblock this tick");
            return Ok(());
        }
    };

    if blocked.is_empty() {
        return Ok(());
    }

    // Batch-load all runs for blocked tasks in a single query instead of N individual queries.
    let task_ids: Vec<i64> = blocked.iter().map(|t| t.id).collect();
    let mut runs_by_task = match store.get_runs_batch(&task_ids).await {
        Ok(map) => map,
        Err(e) => {
            tracing::warn!(err = %e, "get_runs_batch failed — skipping auto-unblock this tick");
            return Ok(());
        }
    };

    // Separate CI-failure blocked tasks from other blocked tasks.
    let (ci_failure_tasks, other_blocked): (Vec<_>, Vec<_>) =
        blocked.into_iter().partition(is_ci_failure_block);

    // Handle CI-failure blocked tasks by verifying PR state.
    if !ci_failure_tasks.is_empty() {
        let gh = match GhHttp::new() {
            Ok(g) => Some(g),
            Err(e) => {
                tracing::warn!(err = %e, "failed to create GhHttp — skipping CI failure unblock this tick");
                None
            }
        };
        for task in ci_failure_tasks {
            if ci_failure_unblock_cooldown_elapsed(&task)
                && let Some(ref gh) = gh
                && let Err(e) = try_unblock_ci_failure_task(
                    gh,
                    repo,
                    task_manager,
                    store,
                    &task,
                    auto_merge_in_flight,
                )
                .await
            {
                tracing::warn!(task_id = task.id, err = %e, "CI failure unblock check failed");
            }
        }
    }

    // Process regular blocked tasks (those without a block_reason).
    for task in other_blocked {
        if task.block_reason.is_some() {
            continue;
        }

        let task_id_str = task
            .external_id
            .clone()
            .unwrap_or_else(|| format!("internal:{}", task.id));
        let dispatch_key = format!("{repo}/{task_id_str}");
        if dispatching.contains_key(&dispatch_key) {
            continue;
        }

        let runs = runs_by_task.remove(&task.id).unwrap_or_default();
        let relevant_runs: Vec<_> = runs
            .into_iter()
            .filter(|r| r.run_type == "agent" || r.run_type == "review")
            .collect();
        let mut failures = Vec::new();
        let mut has_review_failure = false;
        for run in relevant_runs.iter().rev() {
            if let Some(category) = classify_failure_from_run(run) {
                if run.run_type == "review" {
                    has_review_failure = true;
                }
                failures.push(category);
            }
            if failures.len() >= 3 {
                break;
            }
        }

        if failures.is_empty() || !failures.iter().all(|f| f.is_recoverable()) {
            continue;
        }

        // Determine the most recent failure category as the "reason key" for this unblock.
        let reason_key = failures.first().map_or("", |f| f.as_str());

        // Detect reason changes to reset exponential backoff.
        // New reasons bypass old cooldown windows and start fresh attempt counters.
        let current_reason = task.auto_unblock_last_reason.clone();
        let reason_changed = reason_key != current_reason;

        // Compute effective counter: when the reason changes, start from 0;
        // otherwise use the accumulated count for exponential backoff.
        let cooldown_count = if reason_changed {
            0
        } else {
            task.auto_unblock_count
        };

        // Gate on attempt count using the effective count — if the reason changed, the
        // counter was reset above so cooldown_count is 0 and a new reason always gets a
        // fresh attempt regardless of how many times the old reason fired.
        if cooldown_count >= 3 {
            continue;
        }

        if !auto_unblock_cooldown_elapsed(cooldown_count, &task.auto_unblock_last_at) {
            continue;
        }

        // Compute the new counter value: when reason changed, cooldown_count is 0,
        // so incrementing gives 1 (first attempt for the new reason). When the reason is the
        // same, advance from current count for exponential backoff.
        let new_count = cooldown_count + 1;
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

        let ext_id = task
            .external_id
            .clone()
            .unwrap_or_else(|| format!("internal:{}", task.id));
        let new_status = if has_review_failure {
            Status::NeedsReview
        } else {
            Status::New
        };
        let new_status_str = if has_review_failure {
            "needs_review"
        } else {
            "new"
        };

        // First update the status; only increment the counter if this succeeds.
        // This prevents transient GitHub API errors from exhausting the retry quota.
        if let Err(e) = task_manager
            .update_task_status(&ExternalId(ext_id), new_status)
            .await
        {
            tracing::warn!(
                task_id = task.id,
                err = %e,
                "auto-unblock failed to update status — counter not incremented"
            );
            continue;
        }

        // Status update succeeded; now record the counter increment and clear related fields.
        let mut fields: Vec<(&str, serde_json::Value)> = vec![
            ("auto_unblock_count", serde_json::json!(new_count)),
            ("auto_unblock_last_at", serde_json::json!(now)),
            ("auto_unblock_last_reason", serde_json::json!(reason_key)),
            ("agent", serde_json::Value::Null),
            ("model", serde_json::Value::Null),
        ];
        fields.push(("no_code_reroutes", serde_json::json!(0)));
        fields.push(("no_code_last_agent", serde_json::json!("")));
        if has_review_failure {
            fields.push(("review_agent_failures", serde_json::json!(0)));
            fields.push(("review_invocations", serde_json::json!(0)));
            fields.push(("review_resets", serde_json::json!(0)));
        }
        if let Err(e) = store.set_fields(task.id, &fields).await {
            tracing::warn!(task_id = task.id, err = %e, "failed to set auto_unblock fields after successful status update");
            // Task is unblocked (correct behavior) but counter hasn't advanced.
            // Slightly over-eager retries are better than permanent task stalls.
        }

        if let Err(e) = store
            .append_activity(
                task.id,
                "auto_unblock",
                Some("blocked"),
                Some(new_status_str),
                None,
                None,
                Some(&serde_json::json!({
                    "reason": reason_key,
                    "count": if reason_changed { 1 } else { task.auto_unblock_count + 1 },
                    "failures": failures.iter().map(|f| format!("{f:?}")).collect::<Vec<_>>(),
                })),
            )
            .await
        {
            tracing::warn!(task_id = task.id, err = %e, "failed to log auto_unblock activity");
        }

        tracing::info!(
            task_id = task.id,
            failures = ?failures,
            review_failure = has_review_failure,
            "auto-unblocked task with recoverable failures"
        );
    }

    Ok(())
}

/// Global sweep for CI-failure blocked tasks from inactive or removed projects.
///
/// `auto_unblock_blocked_tasks` is called from `sync_tick` and only runs for the
/// currently-active repo. Tasks that were blocked for a project that is later removed
/// from orch config never get processed and accumulate in the DB indefinitely.
///
/// This function queries all blocked tasks across all repos and handles merge-time
/// CI-failure cases (`during auto-merge` or `CI checks timed out`) so they are
/// eventually cleaned up regardless of project activity. Pre-review CI-failure
/// blocks stay blocked for a person. Called once per main tick from
/// `tick_global_sweeps`.
pub(crate) async fn auto_unblock_ci_failure_blocked_tasks_global(
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    auto_merge_in_flight: &Arc<DashSet<String>>,
) -> anyhow::Result<()> {
    let all_blocked = match store.list_all_by_status_global(TaskStatus::Blocked).await {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(err = %e, "global CI-failure blocked sweep: failed to list blocked tasks");
            return Ok(());
        }
    };

    let ci_failure_tasks: Vec<_> = all_blocked
        .into_iter()
        .filter(is_ci_failure_block)
        .collect();
    if ci_failure_tasks.is_empty() {
        return Ok(());
    }

    let gh = match GhHttp::new() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(err = %e, "global CI-failure blocked sweep: failed to create GhHttp");
            return Ok(());
        }
    };

    for task in ci_failure_tasks {
        if !ci_failure_unblock_cooldown_elapsed(&task) {
            continue;
        }
        if let Err(e) = try_unblock_ci_failure_task(
            &gh,
            &task.repo,
            task_manager,
            store,
            &task,
            auto_merge_in_flight,
        )
        .await
        {
            tracing::warn!(
                task_id = task.id,
                repo = %task.repo,
                err = %e,
                "global CI-failure blocked sweep: unblock check failed"
            );
        }
    }

    Ok(())
}

/// Global sweep: re-fire stale NeedsReview tasks and escalate persistently-stuck ones
/// to Blocked, across all repos.
///
/// Two cases where the event-driven subscriber in `subscribers/review.rs` can miss
/// a NeedsReview task: all review slots are busy when the event fires, or the
/// broadcast receiver lagged and dropped it. Re-publish the NeedsReview status
/// after a short idle window so the subscriber can pick it up when a slot is free.
/// Using a status re-fire rather than spawning the review agent directly keeps this
/// path stateless. The subscriber is still the sole spawner and uses the InReview
/// transition as its atomic guard (issue #857)
///
/// Queries across all repos and writes through the store via `*_by_store_id`
/// TaskManager helpers. Called once per main tick from `tick_global_sweeps`
pub(crate) async fn refire_and_escalate_stale_needs_review_global(
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    router: &Router,
    dispatching: &Arc<DashMap<String, String>>,
    active_repos: &HashSet<String>,
) -> anyhow::Result<()> {
    const MIN_STALE_NEEDS_REVIEW_MINUTES: i64 = 1;
    const MAX_NEEDS_REVIEW_REFIRE_ATTEMPTS: u64 = 5;

    let needs_review_tasks = match store
        .list_all_by_status_global(TaskStatus::NeedsReview)
        .await
    {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(err = %e, "global NeedsReview refire sweep: failed to list needs_review tasks");
            return Ok(());
        }
    };

    if needs_review_tasks.is_empty() {
        return Ok(());
    }

    // Re-firing while every review agent is cooled wastes the counter and can
    // escalate a task to Blocked while the subscriber would only bail again
    let all_review_agents_cooled = router.healthy_agent_count("review") == 0;
    if all_review_agents_cooled {
        tracing::info!(
            count = needs_review_tasks.len(),
            "global NeedsReview refire sweep: no review agent available — skipping this pass"
        );
        return Ok(());
    }

    for stored in needs_review_tasks {
        // No review subscriber listens for a repo outside the active set
        if !active_repos.contains(&stored.repo) {
            continue;
        }

        let external = tasks::store_task_to_external(&stored);
        let ext_id = external.id.clone();
        let repo = stored.repo.clone();

        let age_minutes = match chrono::DateTime::parse_from_rfc3339(&external.updated_at) {
            Ok(updated_at) => {
                let age = chrono::Utc::now() - updated_at.with_timezone(&chrono::Utc);
                if age.num_minutes() < MIN_STALE_NEEDS_REVIEW_MINUTES {
                    continue;
                }
                Some(age.num_minutes())
            }
            Err(_) => None,
        };

        // Skip tasks actively being dispatched by their owning repo's engine loop.
        let dispatch_key = format!("{}/{}", repo, ext_id.0);
        if dispatching.contains_key(&dispatch_key) {
            continue;
        }

        let current_refires = u64::try_from(stored.needs_review_refires).unwrap_or(0);
        let new_refires = current_refires + 1;

        if new_refires > MAX_NEEDS_REVIEW_REFIRE_ATTEMPTS {
            tracing::warn!(
                task_id = stored.id,
                repo = %repo,
                new_refires,
                "global NeedsReview refire sweep: escalating to Blocked after repeated refires"
            );
            let fields = [
                ("block_reason", serde_json::json!(REBROADCAST_BLOCK_REASON)),
                (
                    "last_error",
                    serde_json::json!(format!("escalated after {} retries", new_refires)),
                ),
            ];
            if let Err(e) = task_manager
                .update_task_status_and_result_by_store_id(
                    stored.id,
                    &repo,
                    &ext_id,
                    Status::Blocked,
                    &fields,
                )
                .await
            {
                tracing::error!(task_id = stored.id, repo = %repo, err = %e, "global NeedsReview refire sweep: escalation to Blocked failed — skipping to avoid silent auto-unblock loop");
                continue;
            }
            if let Err(e) = store::store_increment_by_id(
                &Some(Arc::clone(store)),
                stored.id,
                "needs_review_refires",
            )
            .await
            {
                tracing::warn!(
                    task_id = stored.id,
                    repo = %repo,
                    err = %e,
                    "global NeedsReview refire sweep: failed to increment needs_review_refires after escalation"
                );
            }
            continue;
        }

        // Exponential backoff: MIN * 2^(current_refires)
        let required_minutes =
            MIN_STALE_NEEDS_REVIEW_MINUTES * (1i64 << u32::try_from(current_refires).unwrap_or(0));

        let should_fire = match age_minutes {
            Some(age) => age >= required_minutes,
            None => true,
        };

        if !should_fire {
            continue;
        }

        if let Err(e) = task_manager
            .update_task_status_by_store_id(stored.id, &repo, &ext_id, Status::NeedsReview)
            .await
        {
            tracing::warn!(
                task_id = stored.id,
                repo = %repo,
                err = %e,
                "global NeedsReview refire sweep: failed to re-fire NeedsReview event — not incrementing counter"
            );
            continue;
        }

        let fired_refires = match store::store_increment_by_id(
            &Some(Arc::clone(store)),
            stored.id,
            "needs_review_refires",
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    task_id = stored.id,
                    repo = %repo,
                    err = %e,
                    "global NeedsReview refire sweep: failed to increment needs_review_refires after re-fire"
                );
                current_refires + 1
            }
        };

        tracing::info!(
            task_id = stored.id,
            repo = %repo,
            age_minutes,
            refires = fired_refires,
            required_minutes,
            "global NeedsReview refire sweep: re-firing NeedsReview event for stale task"
        );
    }

    Ok(())
}

/// Sync tick — runs every 45s.
///
/// Handles less-frequent operations:
/// - Cleanup finished worktrees
/// - Check for merged PRs → mark tasks done
/// - Scan for @mentions
#[expect(
    clippy::too_many_arguments,
    reason = "the engine passes its shared handles one by one"
)]
pub(crate) async fn sync_tick(
    backend: &Arc<dyn ExternalBackend>,
    tmux: &Arc<TmuxManager>,
    repo: &str,
    config: &EngineConfig,
    router: &Arc<RwLock<Router>>,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    dispatching: &Arc<DashMap<String, String>>,
    auto_merge_in_flight: &Arc<DashSet<String>>,
) -> anyhow::Result<()> {
    // One node polls GitHub and decides for the whole cluster
    if !store.is_leader() {
        return Ok(());
    }

    tracing::debug!("sync tick");

    let mut phases: Vec<(&str, u128)> = Vec::new();
    let mut at = Instant::now();

    // Global GitHub 5xx circuit breaker — skip non-critical background work
    // during sustained GitHub outages to reduce churn and wasted API calls.
    let gh_circuit_open = is_github_circuit_open();

    if gh_circuit_open {
        let remaining = github_circuit_remaining_secs();
        tracing::debug!(
            remaining_secs = remaining,
            "GitHub 5xx circuit breaker open — skipping non-critical sync work"
        );
        // Persist circuit breaker state as a metric for operators.
        // This is critical for avoiding thundering herd after restart during outages.
        if let Err(e) =
            try_kv_set_prefer_store(Some(store), "metrics:orch.github_5xx_circuit.open", "1").await
        {
            tracing::error!(err = %e, "failed to persist circuit breaker state — may cause thundering herd after restart");
        }
    } else if let Err(e) =
        try_kv_set_prefer_store(Some(store), "metrics:orch.github_5xx_circuit.open", "0").await
    {
        tracing::error!(err = %e, "failed to clear circuit breaker state");
    }

    // 0. Ingest all active external tasks into the unified store.
    //    This ensures the store has data for tasks created before dual-write was added.
    if !gh_circuit_open {
        if let Err(e) = ingest_external_tasks(backend, repo, store).await {
            tracing::debug!(err = %e, "external task ingest failed");
        }
        phases.push(("ingest", at.elapsed().as_millis()));
        at = Instant::now();

        // Reconcile existing board members too: labels can change without a
        // local task transition, and a human/automation may move a card. The
        // backend only writes drifted fields through the shared GitHub client.
        if let Err(e) = backend.reconcile_project_statuses().await {
            tracing::warn!(err = %e, "project status reconciliation failed");
        }
        phases.push(("board_reconcile", at.elapsed().as_millis()));
    }
    at = Instant::now();

    // 1. Cleanup worktrees for done tasks (background — must not block routing/dispatch)
    if !gh_circuit_open {
        let backend = Arc::clone(backend);
        let repo = repo.to_string();
        let task_manager = Arc::clone(task_manager);
        let store = Arc::clone(store);
        tokio::spawn(async move {
            if let Err(e) = cleanup_done_worktrees(&backend, &repo, &task_manager, &store).await {
                tracing::warn!(err = %e, "worktree cleanup failed");
            }
            if let Err(e) = prune_old_run_output(&store).await {
                tracing::warn!(err = %e, "task run pruning failed");
            }
        });
    }

    let (in_review_tasks, needs_review_tasks, blocked_review_candidates) =
        match prefetch_review_tasks(store, repo).await {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(err = %e, "failed to prefetch review tasks");
                (vec![], vec![], vec![])
            }
        };
    phases.push(("prefetch_review", at.elapsed().as_millis()));
    at = Instant::now();

    if !gh_circuit_open {
        let mention_fallback = chrono::Utc::now() - chrono::Duration::hours(24);
        let mention_since = kv_get_prefer_store(Some(store), "mentions_last_checked")
            .await
            .unwrap_or_else(|| mention_fallback.format("%Y-%m-%dT%H:%M:%SZ").to_string());
        let shared_since = mention_since.clone();

        let cached_router_config = router.read().await.config.clone();
        // Include blocked tasks that have branch/pr info when checking for merged PRs.
        let mut combined_needs = needs_review_tasks.clone();
        combined_needs.extend(blocked_review_candidates.clone());

        let (merge_result, comments_result, review_result) = tokio::join!(
            check_merged_prs(
                backend,
                repo,
                store,
                task_manager,
                &in_review_tasks,
                &combined_needs,
            ),
            fetch_comments_since(backend, &shared_since),
            async {
                let gh = match GhHttp::new() {
                    Ok(g) => g,
                    Err(e) => {
                        tracing::warn!(err = %e, "failed to create GitHub HTTP client for review poll");
                        return Ok(());
                    }
                };
                Box::pin(review_open_prs(
                    backend,
                    repo,
                    config,
                    task_manager,
                    store,
                    dispatching,
                    auto_merge_in_flight,
                    &in_review_tasks,
                    &gh,
                    &cached_router_config,
                ))
                .await
            }
        );

        if let Err(e) = merge_result {
            tracing::warn!(err = %e, "PR merge check failed");
        }

        match comments_result {
            Ok(comments) => {
                let filtered = filter_mentions_by_since(&comments, &mention_since);

                if let Err(e) =
                    scan_comments(backend, Some(store), repo, task_manager, Some(&filtered)).await
                {
                    tracing::warn!(err = %e, "comment scan failed");
                }
            }
            Err(e) => {
                tracing::warn!(err = %e, "comment fetch failed");
            }
        }

        if let Err(e) = review_result {
            tracing::warn!(err = %e, "PR review failed");
        }

        if let Err(e) = ci_bootstrap::apply_pending(store, task_manager, repo).await {
            tracing::warn!(err = %e, "CI bootstrap ruleset apply failed");
        }

        phases.push(("merge_comments_review", at.elapsed().as_millis()));
    }
    at = Instant::now();

    // 5. Stale InReview recovery lives in `tick_recover_stuck_tasks`, the single owner of an
    // in_review task whose review session is gone or hung. It ran here too, after one minute,
    // which beat a finished review's result to the engine and dropped it as stale

    // 5c. Detect stale InProgress tasks and recover them.
    //
    // Similar to stale InReview detection, we check for InProgress tasks whose
    // agent tmux session has died (crash, OOM, etc.) and reset them to Routed
    // so they can be re-dispatched.
    let in_progress_tasks: Vec<_> = match store.list_by_status(repo, TaskStatus::InProgress).await {
        Ok(tasks) => tasks
            .into_iter()
            .map(|stored| {
                let external = tasks::store_task_to_external(&stored);
                (stored, external)
            })
            .collect(),
        Err(e) => {
            tracing::warn!(err = %e, "failed to list in_progress tasks — skipping stale InProgress check");
            vec![]
        }
    };

    if !in_progress_tasks.is_empty() {
        // Fetch all live tmux sessions once instead of one subprocess call per task.
        let live_sessions: HashSet<String> = tmux
            .list_sessions()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.name)
            .collect();

        for (_stored, external) in &in_progress_tasks {
            // A task is only considered stale if it has been in InProgress for > 5 minutes.
            const MIN_STALE_MINUTES: i64 = 5;

            let task_id = &external.id.0;

            // Skip tasks currently being processed by the main tick (dispatch + agent flow).
            let dispatch_key = format!("{repo}/{task_id}");
            {
                if dispatching.contains_key(&dispatch_key) {
                    tracing::debug!(
                        task_id = %task_id,
                        "task locked by dispatch flow, skipping stale check"
                    );
                    continue;
                }
            }

            // Skip tasks that just transitioned to InProgress — allow time for the
            // agent to start its tmux session before treating it as stale.
            match chrono::DateTime::parse_from_rfc3339(&external.updated_at) {
                Ok(updated_at) => {
                    let age = chrono::Utc::now() - updated_at.with_timezone(&chrono::Utc);
                    if age.num_minutes() < MIN_STALE_MINUTES {
                        tracing::debug!(
                            task_id = %task_id,
                            age_seconds = age.num_seconds(),
                            "InProgress task is too young to be considered stale, skipping"
                        );
                        continue;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task_id,
                        ts = %external.updated_at,
                        err = %e,
                        "invalid updated_at timestamp — treating task as potentially stale"
                    );
                }
            }

            let agent_session = tmux.session_name(repo, task_id);
            if live_sessions.contains(&agent_session) {
                // Agent is still alive — skip.
                continue;
            }

            // No agent session exists. The agent is dead regardless of
            // session tracking — hard restart, crash, or flag not persisted.
            // Reset to Routed so the dispatcher can re-spawn it.
            {
                tracing::warn!(
                    task_id = %task_id,
                    session = %agent_session,
                    "InProgress task has no active agent session — resetting to Routed"
                );
                match task_manager
                    .update_task_status_if(&external.id, Status::Routed, Status::InProgress)
                    .await
                {
                    Err(e) => {
                        tracing::error!(task_id = %task_id, err = %e, "failed to reset stale InProgress task — task may be stuck in InProgress indefinitely");
                    }
                    Ok(false) => {
                        tracing::debug!(
                            task_id = %task_id,
                            "stale InProgress reset skipped — task already transitioned (concurrent Done/Blocked/NeedsReview)"
                        );
                    }
                    Ok(true) => {
                        // Successfully reset to Routed
                    }
                }
            }
        }
    }

    phases.push(("stale_in_progress", at.elapsed().as_millis()));
    at = Instant::now();
    // 5c. Auto-unblock tasks with recoverable failures.
    if let Err(e) =
        auto_unblock_blocked_tasks(repo, task_manager, store, dispatching, auto_merge_in_flight)
            .await
    {
        tracing::warn!(err = %e, "auto-unblock failed");
    }

    phases.push(("auto_unblock", at.elapsed().as_millis()));

    engine::log_phase_timings("sync", repo, &phases);
    Ok(())
}

/**
 * Agent health is the same whichever project asks, so it runs once per sync pass rather than
 * once per project. Called after every project's `sync_tick`
 **/
pub(crate) async fn sync_shared_tail(router: &Arc<RwLock<Router>>, store: Option<&Arc<TaskStore>>) {
    let Some(store) = store else {
        return;
    };
    if !store.is_leader() {
        return;
    }

    // Pre-emptive health check: refresh degraded-agent flags from the rate_limits table.
    // Clone what is needed out of the router first, so the write lock is not starved for the
    // duration of the DB query
    let (available_agents, config) = {
        let r = router.read().await;
        (r.available_agents.clone(), r.config.clone())
    };
    let model_checker = |agent: &str| -> bool {
        for comp in &["simple", "medium", "complex", "review"] {
            if config.has_available_model_for_complexity(agent, comp) {
                return true;
            }
        }
        false
    };
    refresh_degraded_agents(
        store,
        &available_agents,
        &model_checker,
        router_config::health_check_window_hours(),
        router_config::degraded_rate_limit_threshold(),
    )
    .await;

    emit_degraded_agents_if_needed(&available_agents, &config, Some(store)).await;
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
async fn scan_comments(
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

/// Per-agent degradation detail collected during the alerting check.
struct DegradedAgentDetail {
    /// Agent name (e.g. "claude", "codex").
    agent: String,
    /// Reason the agent is degraded: the stored cooldown reason when the agent
    /// itself is in cooldown, or `"no_available_model"` when all model pools
    /// are individually cooled.
    reason: String,
    /// Models that are individually in cooldown for this agent, deduped across
    /// all complexity tiers.
    cooled_models: Vec<String>,
}

/// Per-agent pool health used for stale model pool detection.
struct AgentPoolHealth {
    agent: String,
    cooled_models: Vec<String>,
    persistent_failed_models: Vec<String>,
    total_configured_models: usize,
}

/// Inspect router & cooldown state and emit metrics every tick.
///
/// - `metrics:orch.agents_degraded.count` is written on every call with the
///   current number of degraded agents (0 when healthy).
/// - When `count >= 3` a WARN log is emitted with full dimensions (agent
///   names, cooled models, cooldown reasons) and a dedicated alert metric
///   `metrics:orch.agents_degraded.alert` is set to `"1"`. The alert is
///   cleared to `"0"` when the count drops below the threshold.
pub(crate) async fn emit_degraded_agents_if_needed(
    available_agents: &[String],
    config: &RouterConfig,
    store: Option<&Arc<TaskStore>>,
) {
    // Edge-triggered: only log when the affected-agents set changes, so a
    // persistent condition doesn't flood the log every sync tick.
    const STALE_AGENTS_KV: &str = "metrics:orch.model_pool_stale_persistent.agents";

    // Complexity tiers to check for configured models.
    const COMPLEXITIES: &[&str] = &["simple", "medium", "complex", "review"];
    const HEAVILY_COOLED_RATIO_THRESHOLD: f64 = 0.5;

    let mut details: Vec<DegradedAgentDetail> = Vec::new();
    let mut pool_health: Vec<AgentPoolHealth> = Vec::new();

    for agent in available_agents {
        let agent_in_cd = is_agent_in_cooldown(agent);

        // Collect individually cooled models across all complexity tiers (deduped).
        let mut cooled_models: Vec<String> = Vec::new();
        let mut persistent_failed_models: Vec<String> = Vec::new();
        let mut seen_models = HashSet::new();
        for comp in COMPLEXITIES {
            if let Some(pool) = config.model_pool_for_complexity(agent, comp) {
                for model in pool {
                    if seen_models.insert(model.clone()) && is_model_in_cooldown(agent, &model) {
                        let key = format!("{agent}:{model}");
                        if matches!(
                            cooldown_reason(&key).as_deref(),
                            Some("persistent_model_error" | "model_not_found")
                        ) {
                            persistent_failed_models.push(model.clone());
                        }
                        cooled_models.push(model);
                    }
                }
            }
        }
        let total_configured_models = seen_models.len();
        pool_health.push(AgentPoolHealth {
            agent: agent.clone(),
            cooled_models: cooled_models.clone(),
            persistent_failed_models: persistent_failed_models.clone(),
            total_configured_models,
        });

        // An agent is degraded if it is in agent-level cooldown OR has no
        // available (non-cooled) model across any complexity tier.
        let has_model = COMPLEXITIES
            .iter()
            .any(|comp| config.has_available_model_for_complexity(agent, comp));

        if agent_in_cd || !has_model {
            let reason = if agent_in_cd {
                cooldown_reason(agent).unwrap_or_else(|| "agent_cooldown".to_string())
            } else {
                "no_available_model".to_string()
            };
            details.push(DegradedAgentDetail {
                agent: agent.clone(),
                reason,
                cooled_models,
            });
        }
    }

    let count = details.len();

    // Always persist the count metric so operators can scrape it every tick,
    // including when the count is 0.
    if let Err(e) = try_kv_set_prefer_store(
        store,
        "metrics:orch.agents_degraded.count",
        &count.to_string(),
    )
    .await
    {
        tracing::warn!(err = %e, "failed to persist degraded agents count metric");
    }

    if count >= 3 {
        // Build structured dimension strings for the log.
        let degraded_agents: Vec<&str> = details.iter().map(|d| d.agent.as_str()).collect();
        let cooled_models_dim: Vec<String> = details
            .iter()
            .filter(|d| !d.cooled_models.is_empty())
            .map(|d| format!("{}:[{}]", d.agent, d.cooled_models.join(",")))
            .collect();
        let reasons_dim: Vec<String> = details
            .iter()
            .map(|d| format!("{}={}", d.agent, d.reason))
            .collect();

        tracing::warn!(
            degraded_count = count,
            degraded_agents = ?degraded_agents,
            cooled_models = %cooled_models_dim.join("; "),
            cooldown_reasons = %reasons_dim.join("; "),
            "multi-agent degradation detected"
        );

        // Dedicated alert metric: "1" while the threshold is exceeded.
        if let Err(e) =
            try_kv_set_prefer_store(store, "metrics:orch.agents_degraded.alert", "1").await
        {
            tracing::warn!(err = %e, "failed to persist degraded agents alert metric");
        }
    } else if let Err(e) =
        try_kv_set_prefer_store(store, "metrics:orch.agents_degraded.alert", "0").await
    {
        // Clear the alert metric when healthy.
        tracing::warn!(err = %e, "failed to clear degraded agents alert metric");
    }

    // Additional operator signal for dead/stale model pools:
    // alert when any agent has at least 50% of configured models cooled and at
    // least one persistent model failure marker in cooldown reason.
    let heavily_cooled_agents: Vec<String> = pool_health
        .iter()
        .filter_map(|d| {
            if d.total_configured_models == 0 || d.persistent_failed_models.is_empty() {
                return None;
            }
            #[expect(
                clippy::cast_precision_loss,
                reason = "model counts are far below 2^52"
            )]
            let ratio = d.cooled_models.len() as f64 / d.total_configured_models as f64;
            (ratio >= HEAVILY_COOLED_RATIO_THRESHOLD).then(|| {
                format!(
                    "{}:{}/{}:{}",
                    d.agent,
                    d.cooled_models.len(),
                    d.total_configured_models,
                    d.persistent_failed_models.join(",")
                )
            })
        })
        .collect();

    let current_dim = heavily_cooled_agents.join(",");
    let prev_dim = kv_get_prefer_store(store, STALE_AGENTS_KV)
        .await
        .unwrap_or_default();

    if heavily_cooled_agents.is_empty() {
        if !prev_dim.is_empty() {
            tracing::info!("agent model pool stale condition cleared");
        }
        if let Err(e) = try_kv_set_prefer_store(store, STALE_AGENTS_KV, "").await {
            tracing::warn!(err = %e, "failed to clear stale model pool agents state");
        }
        if let Err(e) =
            try_kv_set_prefer_store(store, "metrics:orch.model_pool_stale_persistent.alert", "0")
                .await
        {
            tracing::warn!(err = %e, "failed to clear stale model pool alert metric");
        }
    } else {
        if current_dim != prev_dim {
            tracing::warn!(
                affected_agents = ?heavily_cooled_agents,
                "agent model pool appears stale: persistent model failures in heavily cooled pool"
            );
        }
        if let Err(e) = try_kv_set_prefer_store(store, STALE_AGENTS_KV, &current_dim).await {
            tracing::warn!(err = %e, "failed to persist stale model pool agents state");
        }
        if let Err(e) =
            try_kv_set_prefer_store(store, "metrics:orch.model_pool_stale_persistent.alert", "1")
                .await
        {
            tracing::warn!(err = %e, "failed to persist stale model pool alert metric");
        }
    }
}

/// Ingest all active external tasks into the unified SQLite store.
///
/// Upserts each task so the store stays in sync with the backend.
/// Fetches all open issues (including unlabeled ones) so newly created
/// issues appear in the store immediately, not only after they get a
/// `status:*` label.  This is best-effort — individual task failures
/// are logged and skipped.
pub(crate) async fn ingest_external_tasks(
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    store: &Arc<TaskStore>,
) -> anyhow::Result<()> {
    #[derive(Clone)]
    struct DuplicateTarget {
        id: String,
        status_label: String,
    }

    let mut existing_by_title: HashMap<String, DuplicateTarget> = HashMap::new();
    let mut accepted_by_title: HashMap<String, DuplicateTarget> = HashMap::new();

    let cutoff = (chrono::Utc::now() - chrono::Duration::hours(24))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    match store.list_for_doctor(repo, &cutoff).await {
        Ok(recent) => {
            let mut insert_target = |task: &Task| {
                if task.origin == "internal" {
                    return;
                }
                let Some(ext_id) = task.external_id.as_ref() else {
                    return;
                };
                let status_label = task
                    .labels
                    .iter()
                    .find(|l| l.starts_with("status:"))
                    .cloned()
                    .unwrap_or_default();
                existing_by_title
                    .entry(task.title.clone())
                    .or_insert(DuplicateTarget {
                        id: ext_id.clone(),
                        status_label,
                    });
            };

            // Prefer active tasks over recently done ones when selecting a canonical issue.
            for task in recent.iter().filter(|t| t.status != TaskStatus::Done) {
                insert_target(task);
            }
            for task in recent.iter().filter(|t| t.status == TaskStatus::Done) {
                insert_target(task);
            }
        }
        Err(e) => {
            tracing::warn!(repo, err = %e, "ingest: failed to load recent tasks for dedup");
        }
    }

    // Fetch every open issue, with no `since` filter. The GitHub backend overrides
    // list_active_open_issues() to use one list_all_open_issues() request.
    //
    // A `since` cursor keys on updated_at, so an open issue with no task was skipped
    // forever once the cursor passed its updated_at, and the cursor had to be reset,
    // sentinelled and periodically ignored to recover (#3545, #3787, #3792). An
    // unfiltered listing has no such state and is cheaper too: its URL repeats, so
    // `get_all_pages` sends it conditionally and GitHub answers 304 while nothing
    // changed. A 304 does not count against the REST quota, a `since` fetch does.
    let all_tasks: Vec<(ExternalTask, Option<Status>)> =
        match backend.list_active_open_issues().await {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(
                    repo,
                    err = %e,
                    "ingest: failed to list open issues — no new issue is ingested this cycle"
                );
                return Ok(());
            }
        };

    // Upsert into the store, collecting store IDs for batch status check
    // Also acknowledge newly detected issues (eyes reaction).
    let mut id_status_pairs: Vec<(i64, Status)> = Vec::new();
    // Collect (store_id, node_id) for newly ingested tasks so we can batch-fetch
    // estimate values from GitHub Projects after the loop.
    let mut new_task_node_ids: Vec<(i64, String)> = Vec::new();
    // Pre-load existing external IDs for this repo to avoid N+1 queries
    // (each get_by_external_id call is an individual SQL query). If the
    // lookup fails, fall back to an empty set so we conservatively treat
    // tasks as new (they will be upserted below).
    let existing_ext_ids: HashSet<String> = match store.existing_external_ids(repo).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(repo = repo, err = %e, "ingest: failed to load existing external ids — proceeding");
            HashSet::new()
        }
    };
    for (task, status) in &all_tasks {
        // Check if this task already exists in the store before upserting.
        // Use the pre-loaded HashSet to avoid issuing N individual queries.
        let is_new = !existing_ext_ids.contains(&task.id.0);

        if is_new {
            let duplicate_target = existing_by_title
                .get(&task.title)
                .or_else(|| accepted_by_title.get(&task.title));
            if let Some(target) = duplicate_target
                && target.id != task.id.0
            {
                let label = target.status_label.as_str();
                let should_dedupe =
                    match backend.has_open_issue_with_title(&task.title, label).await {
                        Ok(true) => true,
                        Ok(false) => false,
                        Err(e) => {
                            tracing::warn!(
                                task_id = task.id.0,
                                err = %e,
                                "ingest: dedup check failed — defaulting to keep issue"
                            );
                            false
                        }
                    };

                if should_dedupe {
                    let comment = format!(
                        "Duplicate of #{id} — closing to avoid duplicate work.{footer}",
                        id = target.id,
                        footer = engine::orch_footer()
                    );
                    if let Err(e) = backend.post_comment(&task.id, &comment).await {
                        tracing::warn!(
                            task_id = task.id.0,
                            err = %e,
                            "ingest: failed to comment on duplicate issue"
                        );
                    }
                    match backend.update_status(&task.id, Status::Done).await {
                        Ok(()) => {
                            tracing::info!(
                                task_id = task.id.0,
                                duplicate_of = target.id,
                                "ingest: closed duplicate issue"
                            );
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!(
                                task_id = task.id.0,
                                err = %e,
                                "ingest: failed to close duplicate issue — falling through to ingest normally"
                            );
                        }
                    }
                }
            }
        }

        match store.ensure_external_task(repo, task).await {
            Ok(store_id) => {
                if let Ok(number) = task.id.0.parse::<i64>()
                    && let Err(err) = dependencies::sync_issue_dependencies(
                        backend.as_ref(),
                        store,
                        store_id,
                        repo,
                        Some(number),
                        &task.body,
                    )
                    .await
                {
                    tracing::warn!(
                        task_id = task.id.0,
                        err = %err,
                        "ingest: dependency sync failed"
                    );
                }
                if let Some(s) = status {
                    id_status_pairs.push((store_id, *s));
                }
                // Acknowledge newly detected issues with an eyes reaction.
                if is_new {
                    let status_label = task
                        .labels
                        .iter()
                        .find(|l| l.starts_with("status:"))
                        .cloned()
                        .unwrap_or_default();
                    accepted_by_title
                        .entry(task.title.clone())
                        .or_insert(DuplicateTarget {
                            id: task.id.0.clone(),
                            status_label,
                        });
                    if let Err(e) = backend.acknowledge_issue(&task.id).await {
                        tracing::debug!(
                            task_id = task.id.0,
                            err = %e,
                            "ingest: acknowledgment failed"
                        );
                    }
                    // Sync to project board only for newly ingested tasks.
                    // Collect the returned node_id so we can batch-fetch estimates.
                    let task_status = status.unwrap_or(Status::New);
                    match backend.sync_to_project(&task.id, task_status).await {
                        Ok(Some(node_id)) => {
                            new_task_node_ids.push((store_id, node_id));
                        }
                        Ok(None) => {
                            // Project sync not configured or failed — no node_id to collect.
                        }
                        Err(e) => {
                            tracing::debug!(
                                task_id = task.id.0,
                                err = %e,
                                "ingest: project board sync failed"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                tracing::debug!(task_id = task.id.0, ?e, "ingest: upsert failed");
            }
        }
    }

    // Batch-fetch all upserted tasks in a single query, then sync status for NEW tasks.
    // Only sync status from backend → store for NEW tasks (first ingest).
    // Once a task exists in the store, its status is authoritative —
    // re-ingestion must not overwrite store-first status changes
    // (e.g., store has Routed but GitHub still shows New labels).
    if !id_status_pairs.is_empty() {
        let all_ids: Vec<i64> = id_status_pairs.iter().map(|(id, _)| *id).collect();
        let existing_map = match store.get_batch(&all_ids).await {
            Ok(map) => map,
            Err(e) => {
                tracing::warn!(err = %e, task_count = all_ids.len(), "ingest: get_batch failed — skipping initial status sync for newly ingested tasks this tick");
                return Ok(());
            }
        };
        for (store_id, status) in id_status_pairs {
            if let Some(existing) = existing_map.get(&store_id)
                && existing.status == TaskStatus::New
            {
                let db_status = status_to_task_status(status);
                if db_status != TaskStatus::New
                    && let Err(e) = store.update_status(store_id, db_status).await
                {
                    tracing::debug!(?e, "ingest: status sync failed");
                }
            }
        }
    }

    // Batch-fetch estimate values from GitHub Projects for newly ingested tasks.
    // For each task where `tasks.estimate == 0`, populate it from the project board.
    if !new_task_node_ids.is_empty() {
        let node_ids: Vec<String> = new_task_node_ids.iter().map(|(_, n)| n.clone()).collect();
        match backend.get_project_item_estimates(&node_ids).await {
            Ok(estimates) => {
                for (store_id, node_id) in new_task_node_ids {
                    if let Some(&estimate) = estimates.get(&node_id)
                        && estimate > 0
                        && let Err(e) = store.set_estimate_if_zero(store_id, estimate).await
                    {
                        tracing::debug!(
                            store_id,
                            estimate,
                            err = %e,
                            "ingest: failed to set estimate from project"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::debug!(
                    err = %e,
                    "ingest: project estimate fetch failed — skipping estimate sync this tick"
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests;
