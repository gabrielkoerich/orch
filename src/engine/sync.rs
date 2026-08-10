//! Sync tick — periodic operations that run less frequently than the core tick.
//!
//! The sync tick runs every ~45 seconds and handles:
//! - Worktree cleanup for done tasks
//! - Merged-PR detection
//! - @mention scanning and internal task creation
//! - PR review processing and follow-up dispatch
//! - Stale InReview detection
//! - Owner /slash command scanning
//! - Skill repository syncing

use crate::backends::{ExternalBackend, Mention, Status};
use crate::cmd::CommandErrorContext;
use crate::config;
use crate::engine::cooldown::{
    cooldown_reason, github_circuit_remaining_secs, is_agent_in_cooldown, is_github_circuit_open,
    is_model_in_cooldown, refresh_degraded_agents,
};
use crate::engine::router::Router;
use crate::engine::runner::agents::patterns;
use crate::engine::tasks::{status_to_task_status, TaskManager};
use crate::store;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use crate::tmux::TmuxManager;
use dashmap::{DashMap, DashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::RwLock;

static KV_GET_FAILURES: AtomicU64 = AtomicU64::new(0);
static KV_SET_FAILURES: AtomicU64 = AtomicU64::new(0);

#[allow(dead_code)]
pub fn kv_failure_count() -> u64 {
    KV_GET_FAILURES.load(Ordering::Relaxed) + KV_SET_FAILURES.load(Ordering::Relaxed)
}

#[allow(dead_code)]
pub fn reset_kv_failure_count() {
    KV_GET_FAILURES.store(0, Ordering::Relaxed);
    KV_SET_FAILURES.store(0, Ordering::Relaxed);
}

/// Read a KV value from the store.
///
/// Returns `None` if the store is unavailable, the key doesn't exist, OR if
/// the store query fails. Use [`try_kv_get_prefer_store`] if you need to
/// distinguish between "key not found" and actual errors.
async fn kv_get_prefer_store(store: &Option<&Arc<TaskStore>>, key: &str) -> Option<String> {
    match try_kv_get_prefer_store(store, key).await {
        Ok(opt) => opt,
        Err(e) => {
            KV_GET_FAILURES.fetch_add(1, Ordering::Relaxed);
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
    store: &Option<&Arc<TaskStore>>,
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
async fn kv_set_prefer_store(store: &Option<&Arc<TaskStore>>, key: &str, value: &str) {
    if let Err(e) = try_kv_set_prefer_store(store, key, value).await {
        KV_SET_FAILURES.fetch_add(1, Ordering::Relaxed);
        tracing::error!(key, err = %e, "kv_set failed");
    }
}

/// Try to write a KV value to the store, returning errors instead of
/// silently logging them.
///
/// Returns `Ok(())` if the store is unavailable (best-effort) or if the
/// write succeeds. Returns `Err` if the store query fails.
async fn try_kv_set_prefer_store(
    store: &Option<&Arc<TaskStore>>,
    key: &str,
    value: &str,
) -> anyhow::Result<()> {
    if let Some(s) = store {
        s.kv_set(key, value).await?;
    }
    Ok(())
}

use super::cleanup::{check_merged_prs, cleanup_done_worktrees};
use super::commands::{parse_command, validate_and_run_command, CommandOutcome, CommandStoreOps};
use super::review_poll::review_open_prs;
use super::EngineConfig;
use crate::github::types::extract_issue_number_from_url;

#[derive(Debug, Clone)]
pub(crate) struct ReviewTaskSnapshot {
    pub external: crate::backends::ExternalTask,
    pub stored: crate::store::Task,
}

impl ReviewTaskSnapshot {
    fn task_id(&self) -> &str {
        &self.external.id.0
    }
}

async fn prefetch_review_tasks(
    store: &Arc<TaskStore>,
    repo: &str,
) -> anyhow::Result<(
    Vec<ReviewTaskSnapshot>,
    Vec<ReviewTaskSnapshot>,
    Vec<ReviewTaskSnapshot>,
)> {
    let in_review = store
        .list_by_status(repo, TaskStatus::InReview)
        .await?
        .into_iter()
        .map(|stored| ReviewTaskSnapshot {
            external: crate::engine::tasks::store_task_to_external(&stored),
            stored,
        })
        .collect();
    let needs_review = store
        .list_by_status(repo, TaskStatus::NeedsReview)
        .await?
        .into_iter()
        .map(|stored| ReviewTaskSnapshot {
            external: crate::engine::tasks::store_task_to_external(&stored),
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
            external: crate::engine::tasks::store_task_to_external(&stored),
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
                | FailureCategory::MaxAttempts
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

    if outcome == "rate_limit" || lower.contains("rate limit") || lower.contains("usage limit") {
        return FailureCategory::RateLimit;
    }

    // Credit exhaustion is a transient condition (credits replenish, billing cycles
    // reset). Classifying it as recoverable allows auto-unblock to retry once the
    // credit-related cooldown expires, instead of permanently blocking the task.
    if crate::engine::cooldown::detect_credit_exhaustion(&lower).is_some() {
        return FailureCategory::CreditExhausted;
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

fn classify_failure_from_run(run: &crate::store::TaskRun) -> Option<FailureCategory> {
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
    store: &crate::store::TaskStore,
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
        command: crate::engine::commands::OwnerCommand,
        issue_num: String,
    },
    /// @mention with a slash command — execute and record sentinel task.
    ExecuteCommandForMention {
        command: crate::engine::commands::OwnerCommand,
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
        (Some(_), false, None) => CommentAction::Skip,
        // Mention without command — create task for agent
        (None, true, issue_num) => CommentAction::CreateMentionTask { issue_num },
        // Neither mention nor command — skip
        (None, false, _) => CommentAction::Skip,
    }
}

/// Handle a slash command from a mention. Records a sentinel task and advances
/// the cursor on success; logs a warning and returns early if the mention has
/// no valid issue URL.
#[allow(clippy::too_many_arguments)]
/// Execute a slash command mentioned by `@orch` on a comment.
///
/// Returns:
/// - `Ok(true)` — command processed and cursor may continue advancing
/// - `Ok(false)` — mention task insert failed; caller must stop cursor advancement
/// - `Err(_)` — DB lookup failed for parent_id; caller must NOT advance cursor
///   so this mention is retried on the next sync
async fn handle_slash_command(
    backend: &Arc<dyn ExternalBackend>,
    store: Option<&Arc<TaskStore>>,
    repo: &str,
    task_manager: &Arc<TaskManager>,
    gh: &crate::github::http::GhHttp,
    mention: &Mention,
    command: &crate::engine::commands::OwnerCommand,
    cursor: &mut MentionCursor,
) -> anyhow::Result<bool> {
    let issue_num = match mention
        .issue_url
        .as_deref()
        .and_then(extract_issue_number_from_url)
    {
        Some(num) => num,
        None => {
            tracing::warn!(comment_id = %mention.id, "slash command without valid issue number");
            return Ok(true);
        }
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
            // Advance cursor so this comment is not re-processed on the next sync tick.
            // The mention was already acknowledged; the worst case is the command was
            // not executed but the mention is marked as processed — acceptable vs
            // an infinite retry loop.
            cursor.advance(&mention.created_at);
            return Ok(true);
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

fn is_ci_failure_block(task: &crate::store::Task) -> bool {
    let reason = task.block_reason.as_deref().unwrap_or("");
    reason.contains("CI failure limit") || reason.contains("CI checks timed out")
}

fn ci_failure_unblock_cooldown_elapsed(task: &crate::store::Task) -> bool {
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
    gh: &crate::github::http::GhHttp,
    repo: &str,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    task: &crate::store::Task,
) -> anyhow::Result<bool> {
    let pr_number = match task.pr_number {
        Some(n) => n,
        None => {
            tracing::debug!(
                task_id = task.id,
                "CI failure blocked task has no PR number"
            );
            return Ok(false);
        }
    };

    let pr = match gh.get_pr(repo, pr_number as u64).await {
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

    if pr.merged.unwrap_or(false) || pr.state.eq_ignore_ascii_case("closed") {
        tracing::info!(
            task_id = task.id,
            pr_number,
            merged = pr.merged,
            state = %pr.state,
            "PR merged/closed — unblocking CI failure blocked task"
        );
        let ext_id = task
            .external_id
            .clone()
            .unwrap_or_else(|| format!("internal:{}", task.id));
        // Update the store directly by the already-resolved numeric id instead of going
        // through `task_manager.update_task_status`, which resolves by `self.repo` — this
        // sweep runs cross-repo, so `task.repo` may not match the caller's own repo scope.
        task_manager
            .mark_task_done_by_store_id(task.id, &task.repo, &crate::backends::ExternalId(ext_id))
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

    let pr_state = pr.state.as_str();
    let new_reason = format!(
        "CI failure limit reached during auto-merge (PR #{} still open, state: {})",
        pr_number, pr_state
    );
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    // Increment auto_unblock_count so the cooldown backoff (24h, 3d, never) kicks in
    // and we don't hammer the GitHub API every tick for a PR that stays open.
    let new_count = task.auto_unblock_count.saturating_add(1);
    let fields = vec![
        ("block_reason", serde_json::json!(&new_reason)),
        ("auto_unblock_count", serde_json::json!(new_count)),
        ("auto_unblock_last_at", serde_json::json!(&now)),
    ];
    // Use set_fields_silent so the probe write does not bump updated_at on stale
    // blocked tasks — triage queries keyed off updated_at must only see real
    // status/result changes, not periodic re-checks of an open PR.
    store.set_fields_silent(task.id, &fields).await?;

    Ok(false)
}

/// Auto-recover tasks blocked due to refire-counter exhaustion during agent cooldowns.
///
/// When all review agents are cooled simultaneously the NeedsReview refire counter can
/// exhaust (>5 attempts) before any agent becomes available, escalating the task to
/// `Blocked` with `block_reason = "review agent rebroadcast escalated after repeated
/// retries"`. This exact block_reason is written exclusively by refire-counter
/// exhaustion (see the three escalation sites that set it), regardless of how many
/// review cycles preceded it — a task can hit this same cooldown-driven exhaustion on
/// its first review pass (`review_cycles == 0`) or after surviving an earlier
/// `CHANGES_REQUESTED` round and re-entering `NeedsReview` for a second pass
/// (`review_cycles > 0`). The block_reason match alone is a sufficient and exact
/// signal. Once any review agent is routable again, reset them to `NeedsReview` and
/// clear the refire counter so the subscriber can pick them up.
///
/// A minimum block age of `MIN_BLOCK_AGE_MINUTES` is enforced so that tasks escalated
/// in the current tick are not immediately un-blocked by the same tick's recovery pass.
async fn auto_recover_rebroadcast_blocked_tasks(
    repo: &str,
    router: &Arc<RwLock<Router>>,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
) -> anyhow::Result<()> {
    const REBROADCAST_BLOCK_REASON: &str =
        "review agent rebroadcast escalated after repeated retries";
    // Must have been blocked for at least this long before we consider recovering it.
    // Prevents the recovery pass from immediately undoing an escalation that happened
    // earlier in the same sync_tick call.
    const MIN_BLOCK_AGE_MINUTES: i64 = 5;

    // Bail early if no review agents are routable — re-firing while all agents are
    // cooled would immediately re-exhaust the counter.
    let any_routable = {
        let r = router.read().await;
        !r.available_agents.is_empty() && r.healthy_agent_count("review") > 0
    };
    if !any_routable {
        return Ok(());
    }

    let blocked = match store.list_by_status(repo, TaskStatus::Blocked).await {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(err = %e, "failed to list blocked tasks — skipping rebroadcast recovery this tick");
            return Ok(());
        }
    };

    let now = chrono::Utc::now();
    let candidates: Vec<_> = blocked
        .into_iter()
        .filter(|t| {
            if t.block_reason.as_deref() != Some(REBROADCAST_BLOCK_REASON) {
                return false;
            }
            // Only recover tasks that have been blocked long enough to rule out
            // same-tick escalations (i.e. tasks blocked by a previous sync cycle).
            if let Ok(updated_at) = chrono::DateTime::parse_from_rfc3339(&t.updated_at) {
                let age = now - updated_at.with_timezone(&chrono::Utc);
                age.num_minutes() >= MIN_BLOCK_AGE_MINUTES
            } else {
                // Unparseable timestamp → treat as old enough.
                true
            }
        })
        .collect();

    if candidates.is_empty() {
        return Ok(());
    }

    tracing::info!(
        count = candidates.len(),
        "sync: auto-recovering rebroadcast-blocked tasks — review agents now available"
    );

    for task in candidates {
        let ext_id = task
            .external_id
            .clone()
            .unwrap_or_else(|| format!("internal:{}", task.id));

        if let Err(e) = task_manager
            .update_task_status(
                &crate::backends::ExternalId(ext_id.clone()),
                Status::NeedsReview,
            )
            .await
        {
            tracing::warn!(
                task_id = task.id,
                err = %e,
                "rebroadcast-recovery: failed to reset task to needs_review — skipping"
            );
            continue;
        }

        let fields: &[(&str, serde_json::Value)] = &[
            ("block_reason", serde_json::Value::Null),
            ("needs_review_refires", serde_json::json!(0)),
        ];
        if let Err(e) = store.set_fields(task.id, fields).await {
            tracing::warn!(
                task_id = task.id,
                err = %e,
                "rebroadcast-recovery: failed to clear block_reason/needs_review_refires after status reset"
            );
        }

        tracing::info!(
            task_id = task.id,
            ext_id = %ext_id,
            "auto-recovering rebroadcast-blocked task — review agents now available"
        );
    }

    Ok(())
}

async fn auto_unblock_blocked_tasks(
    repo: &str,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    dispatching: &Arc<DashMap<String, String>>,
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
        let gh = match crate::github::http::GhHttp::new() {
            Ok(g) => Some(g),
            Err(e) => {
                tracing::warn!(err = %e, "failed to create GhHttp — skipping CI failure unblock this tick");
                None
            }
        };
        for task in ci_failure_tasks {
            if ci_failure_unblock_cooldown_elapsed(&task) {
                if let Some(ref gh) = gh {
                    if let Err(e) =
                        try_unblock_ci_failure_task(gh, repo, task_manager, store, &task).await
                    {
                        tracing::warn!(task_id = task.id, err = %e, "CI failure unblock check failed");
                    }
                }
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
        let dispatch_key = format!("{}/{}", repo, task_id_str);
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
        let reason_key = failures.first().map(|f| f.as_str()).unwrap_or("");

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
            .update_task_status(&crate::backends::ExternalId(ext_id), new_status)
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
        if failures.contains(&FailureCategory::MaxAttempts) {
            fields.push(("attempts", serde_json::json!(0)));
            fields.push(("route_attempts", serde_json::json!(0)));
        }
        fields.push(("no_code_reroutes", serde_json::json!(0)));
        fields.push(("no_code_last_agent", serde_json::json!("")));
        if has_review_failure {
            fields.push(("review_agent_failures", serde_json::json!(0)));
            fields.push(("review_invocations", serde_json::json!(0)));
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
/// This function queries all blocked tasks across all repos and handles the CI-failure
/// cases (block_reason LIKE '%CI failure%' or '%CI checks timed out%') so they are
/// eventually cleaned up regardless of project activity. Called once per main tick from
/// `tick_recover_stuck_tasks`.
pub(crate) async fn auto_unblock_ci_failure_blocked_tasks_global(
    task_manager: &Arc<crate::engine::tasks::TaskManager>,
    store: &Arc<TaskStore>,
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

    let gh = match crate::github::http::GhHttp::new() {
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
        if let Err(e) =
            try_unblock_ci_failure_task(&gh, &task.repo, task_manager, store, &task).await
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
/// to Blocked, across all repos (not just the currently active project).
///
/// The equivalent per-repo logic in `sync_tick` ("5d. Re-fire events for stale
/// NeedsReview tasks") only runs for entries in `project_engines`, so a task that
/// transitions to NeedsReview for a repo later removed from the active `projects:`
/// list never refires or escalates — it sits in NeedsReview forever with
/// `needs_review_refires` frozen. This mirrors the fix `auto_unblock_ci_failure_blocked_tasks_global`
/// applied for the CI-failure-blocked class: query across all repos and write
/// directly through the store (via the `*_by_store_id` TaskManager helpers) instead
/// of resolving through a repo-scoped `TaskManager`. Called once per main tick from
/// `tick_recover_stuck_tasks`, alongside `auto_unblock_ci_failure_blocked_tasks_global`.
pub(crate) async fn refire_and_escalate_stale_needs_review_global(
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    router: &Router,
    dispatching: &Arc<DashMap<String, String>>,
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

    // Mirrors the per-tick skip in `sync_tick`: if every review agent is currently
    // cooled, re-firing now would waste the refire counter (and risk escalating tasks
    // to Blocked) while the subscriber would only bail again.
    let all_review_agents_cooled =
        !router.available_agents.is_empty() && router.healthy_agent_count("review") == 0;
    if all_review_agents_cooled {
        tracing::info!(
            count = needs_review_tasks.len(),
            "global NeedsReview refire sweep: all review agents currently cooled — skipping this pass"
        );
        return Ok(());
    }

    for stored in needs_review_tasks {
        let external = crate::engine::tasks::store_task_to_external(&stored);
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

        let current_refires = stored.needs_review_refires.max(0) as u64;
        let new_refires = current_refires + 1;

        if new_refires > MAX_NEEDS_REVIEW_REFIRE_ATTEMPTS {
            tracing::warn!(
                task_id = stored.id,
                repo = %repo,
                new_refires,
                "global NeedsReview refire sweep: escalating to Blocked after repeated refires"
            );
            let fields = [
                (
                    "block_reason",
                    serde_json::json!("review agent rebroadcast escalated after repeated retries"),
                ),
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
            if let Err(e) = crate::store::store_increment_by_id(
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

        // Compute required age using exponential backoff: MIN * 2^(current_refires),
        // matching the per-repo `sync_tick` behavior.
        let required_minutes = MIN_STALE_NEEDS_REVIEW_MINUTES * (1i64 << (current_refires as u32));

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

        let fired_refires = match crate::store::store_increment_by_id(
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
#[allow(clippy::too_many_arguments)]
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
    tracing::debug!("sync tick");

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
            try_kv_set_prefer_store(&Some(store), "metrics:orch.github_5xx_circuit.open", "1").await
        {
            tracing::error!(err = %e, "failed to persist circuit breaker state — may cause thundering herd after restart");
        }
    } else if let Err(e) =
        try_kv_set_prefer_store(&Some(store), "metrics:orch.github_5xx_circuit.open", "0").await
    {
        tracing::error!(err = %e, "failed to clear circuit breaker state");
    }

    // 0. Ingest all active external tasks into the unified store.
    //    This ensures the store has data for tasks created before dual-write was added.
    if !gh_circuit_open {
        if let Err(e) = ingest_external_tasks(backend, repo, store).await {
            tracing::debug!(err = %e, "external task ingest failed");
        }
    }

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
        });
    }

    let (mut in_review_tasks, mut needs_review_tasks, blocked_review_candidates) =
        match prefetch_review_tasks(store, repo).await {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(err = %e, "failed to prefetch review tasks");
                (vec![], vec![], vec![])
            }
        };

    if !gh_circuit_open {
        let mention_fallback = chrono::Utc::now() - chrono::Duration::hours(24);
        let mention_since = kv_get_prefer_store(&Some(store), "mentions_last_checked")
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
                let gh = match crate::github::http::GhHttp::new() {
                    Ok(g) => g,
                    Err(e) => {
                        tracing::warn!(err = %e, "failed to create GitHub HTTP client for review poll");
                        return Ok(());
                    }
                };
                review_open_prs(
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
                )
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

        match prefetch_review_tasks(store, repo).await {
            Ok((latest_in_review, latest_needs_review, _latest_blocked_candidates)) => {
                in_review_tasks = latest_in_review;
                needs_review_tasks = latest_needs_review;
            }
            Err(e) => {
                tracing::warn!(err = %e, "failed to refresh review task cache after sync work");
            }
        }
    }

    // 5. Detect stale InReview tasks and recover them.
    //
    // NeedsReview → InReview triggering is handled exclusively by the event-driven
    // subscriber (`src/engine/subscribers/review.rs`). Having the sync tick do the
    // same thing caused a race: both paths could fire simultaneously, each spawning
    // a review agent, each incrementing the failure counter on a single real failure —
    // prematurely blocking tasks at half the expected retry quota (issue #857).
    let enable_review = config::get("workflow.enable_review_agent")
        .map(|v| v != "false")
        .unwrap_or(true);
    if enable_review {
        // Detect stale InReview tasks (review agent crashed, no active tmux session).
        // Read from the store (includes both external and internal tasks).
        // Fetch all live tmux sessions once instead of one subprocess call per task.
        let live_sessions: std::collections::HashSet<String> = tmux
            .list_sessions()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.name)
            .collect();
        for task in &in_review_tasks {
            // Skip tasks currently being processed by the main tick (dispatch + review flow).
            let dispatch_key = format!("{}/{}", repo, task.task_id());
            {
                if dispatching.contains_key(&dispatch_key) {
                    tracing::debug!(
                        task_id = task.task_id(),
                        "task locked by dispatch flow, skipping stale check"
                    );
                    continue;
                }
            }
            // Skip tasks that just transitioned to InReview — allow time for the
            // review agent to start its tmux session before treating it as stale.
            // A task is only considered stale if it has been in InReview for > 1 minute.
            const MIN_STALE_MINUTES: i64 = 1;
            match chrono::DateTime::parse_from_rfc3339(&task.external.updated_at) {
                Ok(updated_at) => {
                    let age = chrono::Utc::now() - updated_at.with_timezone(&chrono::Utc);
                    if age.num_minutes() < MIN_STALE_MINUTES {
                        tracing::debug!(
                            task_id = task.task_id(),
                            age_seconds = age.num_seconds(),
                            "InReview task is too young to be considered stale, skipping"
                        );
                        continue;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = task.task_id(),
                        ts = %task.external.updated_at,
                        err = %e,
                        "invalid updated_at timestamp — treating task as potentially stale"
                    );
                }
            }

            let review_task_id = format!("{}-review", task.task_id());
            let review_session = tmux.session_name(repo, &review_task_id);
            if live_sessions.contains(&review_session) {
                // Review agent is still alive — skip.
                continue;
            }

            // No review session exists. The review agent is dead regardless of
            // review_session_expected — hard restart, crash, or flag not persisted.
            // Reset to NeedsReview so the subscriber re-dispatches.
            {
                tracing::warn!(
                    task_id = task.task_id(),
                    session = %review_session,
                    "InReview task has no active review session — resetting to NeedsReview"
                );
                // Reset the failure counter: stale-session recovery is an infrastructure
                // event (tmux crash, service restart) not a genuine agent parse failure.
                // Keeping the counter would unfairly consume the quota for the next cycle.
                if let Err(e) = store::store_set_result(
                    &Some(Arc::clone(store)),
                    repo,
                    task.task_id(),
                    &[("review_agent_failures", serde_json::json!(0))],
                )
                .await
                {
                    tracing::error!(task_id = task.task_id(), err = %e, "failed to write review_agent_failures to store");
                }
                match task_manager
                    .update_task_status_if(&task.external.id, Status::NeedsReview, Status::InReview)
                    .await
                {
                    Err(e) => {
                        tracing::error!(task_id = task.task_id(), err = %e, "failed to reset stale InReview task — task may be stuck in InReview indefinitely");
                    }
                    Ok(false) => {
                        tracing::debug!(
                            task_id = task.task_id(),
                            "stale InReview reset skipped — task already transitioned (concurrent Done/Blocked/InProgress)"
                        );
                    }
                    Ok(true) => {
                        store::set_review_session_expected(store, repo, task.task_id(), false)
                            .await;
                    }
                }
            }
        }
    }

    // 5c. Detect stale InProgress tasks and recover them.
    //
    // Similar to stale InReview detection, we check for InProgress tasks whose
    // agent tmux session has died (crash, OOM, etc.) and reset them to Routed
    // so they can be re-dispatched.
    let in_progress_tasks: Vec<_> = match store.list_by_status(repo, TaskStatus::InProgress).await {
        Ok(tasks) => tasks
            .into_iter()
            .map(|stored| {
                let external = crate::engine::tasks::store_task_to_external(&stored);
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
        let live_sessions: std::collections::HashSet<String> = tmux
            .list_sessions()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.name)
            .collect();

        for (_stored, external) in &in_progress_tasks {
            let task_id = &external.id.0;

            // Skip tasks currently being processed by the main tick (dispatch + agent flow).
            let dispatch_key = format!("{}/{}", repo, task_id);
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
            // A task is only considered stale if it has been in InProgress for > 5 minutes.
            const MIN_STALE_MINUTES: i64 = 5;
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

    // 5d. Re-fire events for stale NeedsReview tasks.
    //
    // Two cases where the event-driven subscriber in `subscribers/review.rs` can miss
    // a NeedsReview task:
    // 1. All review slots are busy when the NeedsReview event fires — subscriber skips
    //    with `try_acquire_owned()`, task stays in NeedsReview with no future trigger.
    // 2. The broadcast receiver lagged (fell behind) — NeedsReview events were dropped.
    //
    // For both cases: re-publish the NeedsReview status after a short idle window so the
    // subscriber can pick it up when a slot is free. Using `update_task_status` (re-fire)
    // rather than spawning the review agent directly keeps this path stateless and avoids
    // re-introducing the double-trigger race fixed in issue #857 — the subscriber is still
    // the sole spawner and uses the InReview transition as its atomic guard.
    if enable_review {
        const MIN_STALE_NEEDS_REVIEW_MINUTES: i64 = 1;
        const MAX_NEEDS_REVIEW_REFIRE_ATTEMPTS: u64 = 5; // escalate to Blocked after this many refires
        let needs_review_count = needs_review_tasks.len();
        if needs_review_count > 0 {
            tracing::info!(
                count = needs_review_count,
                "sync catch-up: checking stale NeedsReview tasks"
            );
        } else {
            tracing::debug!(
                count = needs_review_count,
                "sync catch-up: checking stale NeedsReview tasks"
            );
        }
        // If every review agent is currently cooled, skip the per-task catch-up
        // pass this tick. Re-firing now would waste the refire counter (and
        // risk escalating tasks to Blocked) while the subscriber would only
        // bail again. The next tick re-checks and resumes re-firing as soon as
        // any review agent recovers.
        let all_review_agents_cooled = {
            let r = router.read().await;
            !r.available_agents.is_empty() && r.healthy_agent_count("review") == 0
        };
        let skip_refires = all_review_agents_cooled && needs_review_count > 0;
        if skip_refires {
            tracing::info!(
                count = needs_review_count,
                "sync catch-up: all review agents currently cooled — skipping re-fire this tick (counter not incremented)"
            );
        }
        for task in &needs_review_tasks {
            if skip_refires {
                break;
            }
            // Only retry tasks that have been in NeedsReview long enough that the
            // subscriber should have handled them by now. Fresh tasks (just transitioned)
            // are likely still in flight via the event bus. Unparseable timestamps are
            // treated as stale (fall through to retry).
            let age_minutes = if let Ok(updated_at) =
                chrono::DateTime::parse_from_rfc3339(&task.external.updated_at)
            {
                let age = chrono::Utc::now() - updated_at.with_timezone(&chrono::Utc);
                if age.num_minutes() < MIN_STALE_NEEDS_REVIEW_MINUTES {
                    continue;
                }
                Some(age.num_minutes())
            } else {
                None
            };

            // Skip tasks actively being dispatched (subscriber is working on them).
            let dispatch_key = format!("{}/{}", repo, task.task_id());
            if dispatching.contains_key(&dispatch_key) {
                continue;
            }
            // Decide whether to re-fire now using exponential backoff based on a per-task counter.
            let current_refires = task.stored.needs_review_refires.max(0) as u64;
            let new_refires = current_refires + 1;

            // If we've exceeded max attempts, escalate the task to Blocked with a clear reason.
            // Allow exactly MAX_NEEDS_REVIEW_REFIRE_ATTEMPTS refires; only escalate when
            // the new_refires value exceeds that limit (fix off-by-one where `>=` would
            // escalate one attempt too early).
            if new_refires > MAX_NEEDS_REVIEW_REFIRE_ATTEMPTS {
                tracing::warn!(
                    task_id = task.task_id(),
                    new_refires,
                    "escalating NeedsReview task to Blocked after repeated refires"
                );
                let fields = [
                    (
                        "block_reason",
                        serde_json::json!(
                            "review agent rebroadcast escalated after repeated retries"
                        ),
                    ),
                    (
                        "last_error",
                        serde_json::json!(format!("escalated after {} retries", new_refires)),
                    ),
                ];
                if let Err(e) = task_manager
                    .update_task_status_and_result(&task.external.id, Status::Blocked, &fields)
                    .await
                {
                    tracing::error!(task_id = task.task_id(), err = %e, "update_task_status_and_result(Blocked) failed — skipping block to avoid silent auto-unblock loop");
                    continue;
                }
                // Increment only after the block transition succeeds — counter reflects actual escalations.
                if let Err(e) = crate::store::store_increment(
                    &Some(Arc::clone(store)),
                    repo,
                    task.task_id(),
                    "needs_review_refires",
                )
                .await
                {
                    tracing::warn!(
                        task_id = task.task_id(),
                        err = %e,
                        "failed to increment needs_review_refires after escalation"
                    );
                }
                continue;
            }

            // Compute required age using exponential backoff: MIN * 2^(current_refires)
            // current_refires == 0 → required == MIN (1 min)
            // current_refires == 1 → required == 2 * MIN (2 min)
            // current_refires == 2 → required == 4 * MIN (4 min) …
            let required_minutes =
                MIN_STALE_NEEDS_REVIEW_MINUTES * (1i64 << (current_refires as u32));

            let should_fire = match age_minutes {
                Some(age) => age >= required_minutes,
                None => true,
            };

            if !should_fire {
                // Not firing yet — do NOT increment the refire counter on mere
                // delay/backoff checks. The counter must reflect *actual* re-fire
                // attempts so it can be used as an escalation quota. Incrementing
                // here inflated the counter when the subscriber was simply still
                // within the backoff window and could wrongly cause escalation.
                tracing::debug!(
                    task_id = task.task_id(),
                    age_minutes,
                    current_refires,
                    required_minutes,
                    "sync catch-up: delaying NeedsReview re-fire due to backoff (no counter increment)"
                );
                continue;
            }

            // Fire: attempt the status update first. Only increment the counter on
            // success so that transient DB failures don't artificially inflate
            // needs_review_refires and cause premature Blocked escalation.
            if let Err(e) = task_manager
                .update_task_status(&task.external.id, Status::NeedsReview)
                .await
            {
                tracing::warn!(
                    task_id = task.task_id(),
                    err = %e,
                    "sync catch-up: failed to re-fire NeedsReview event — not incrementing counter"
                );
                continue;
            }

            // Increment only after the re-fire succeeds (mirrors the escalation path).
            let fired_refires = match crate::store::store_increment(
                &Some(Arc::clone(store)),
                repo,
                task.task_id(),
                "needs_review_refires",
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        task_id = task.task_id(),
                        err = %e,
                        "failed to increment needs_review_refires after re-fire"
                    );
                    current_refires + 1
                }
            };

            tracing::info!(
                task_id = task.task_id(),
                age_minutes,
                refires = fired_refires,
                required_minutes,
                "sync catch-up: re-firing NeedsReview event for stale task"
            );
        }
    }

    // 5c. Auto-unblock tasks with recoverable failures.
    if let Err(e) = auto_unblock_blocked_tasks(repo, task_manager, store, dispatching).await {
        tracing::warn!(err = %e, "auto-unblock failed");
    }

    // 5d. Auto-recover tasks blocked by rebroadcast-escalation once review agents return.
    if enable_review {
        if let Err(e) =
            auto_recover_rebroadcast_blocked_tasks(repo, router, task_manager, store).await
        {
            tracing::warn!(err = %e, "rebroadcast-recovery failed");
        }
    }

    // 6. Sync skill repositories
    match skills_sync().await {
        Ok(()) => {
            // Invalidate the LLM router's skills catalog cache so new/updated
            // skill files are picked up on the next routing call.
            // Clone the Arc handle *before* releasing the read guard so the
            // read lock is not held across the async invalidation call.
            let llm = router.read().await.llm_router_handle();
            llm.invalidate_skills_catalog().await;
        }
        Err(e) => {
            tracing::warn!(err = %e, "skills sync failed");
        }
    }

    // Pre-emptive health check: refresh degraded-agent flags from rate_limits table.
    // Clone the required data out of the router while holding the read guard,
    // then release the guard before the async DB query so the write lock is
    // not starved for the duration of the I/O operation.
    {
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
            crate::engine::router::config::health_check_window_hours(),
            crate::engine::router::config::degraded_rate_limit_threshold(),
        )
        .await;
    }

    Ok(())
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
        Ok(Some(u)) => format!("@{}", u),
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
        let since_str = match kv_get_prefer_store(&store, "mentions_last_checked").await {
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
    let existing_mentions: std::collections::HashSet<String> = if let Some(s) = store {
        match s.list_source_ids_by_source(repo, "mention", 30).await {
            Ok(ids) => ids.into_iter().collect(),
            Err(e) => {
                tracing::warn!(err = %e, "failed to load mention source IDs — skipping to prevent duplicates");
                return Ok(());
            }
        }
    } else {
        std::collections::HashSet::new()
    };

    // Lazily create GhHttp — only allocate when we actually need to make GitHub
    // API calls.  This avoids network initialization in test environments where
    // the code path never reaches an API call.
    let gh_cell: tokio::sync::OnceCell<crate::github::http::GhHttp> = tokio::sync::OnceCell::new();
    let get_gh = || async {
        gh_cell
            .get_or_try_init(|| async { crate::github::http::GhHttp::new() })
            .await
    };

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
                issue_num: Some(ref num),
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
        futures::future::join_all(
            pr_check_inputs
                .iter()
                .map(|(_, num)| async move { gh.is_pull_request(repo, num).await }),
        )
        .await
    };

    // Build a lookup map: comment_index → is_pr.
    let is_pr_map: std::collections::HashMap<usize, bool> = pr_check_inputs
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
                let _outcome = validate_and_run_command(
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
                // Always advance cursor: the outcome comment (success or error) was
                // already posted to GitHub, so re-processing would create duplicates.
                cursor.advance(&comment.created_at);
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
        kv_set_prefer_store(&store, "mentions_last_checked", &ts).await;
    }

    Ok(())
}

/// Sync skill repositories from config.
///
/// Reads the `skills:` list from config and clones/pulls each repository
/// to `~/.orch/skills/{repo}/`. This keeps skill documentation up-to-date
/// for agents.
async fn skills_sync() -> anyhow::Result<()> {
    let skills = match config::get_skills() {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(err = %e, "no skills configured");
            return Ok(());
        }
    };

    if skills.is_empty() {
        tracing::debug!("no skills configured, skipping sync");
        return Ok(());
    }

    let skills_base = crate::home::skills_dir()?;
    let git_timeout = std::time::Duration::from_secs(60);

    // Run all skill git operations concurrently — each is fully independent.
    // Wall-clock time drops from N × timeout to max(timeout).
    let futs: Vec<_> = skills
        .into_iter()
        .map(|skill| sync_one_skill(skill, skills_base.clone(), git_timeout))
        .collect();
    futures::future::join_all(futs).await;

    Ok(())
}

/// Sync a single skill repository: `git pull` if it exists, `git clone` otherwise.
///
/// All errors are logged via `tracing::warn!` so that one failing skill does not
/// prevent the others from being updated.
async fn sync_one_skill(
    skill: config::SkillConfig,
    skills_base: std::path::PathBuf,
    git_timeout: std::time::Duration,
) {
    // Validate repo format to prevent path traversal
    if skill.repo.contains("..") || skill.repo.matches('/').count() != 1 {
        tracing::warn!(repo = %skill.repo, "invalid skill repo format, expected 'owner/repo'");
        return;
    }

    let repo_dir = skills_base.join(&skill.repo);
    let repo_url = format!("https://github.com/{}.git", skill.repo);

    // Use async metadata check instead of `Path::exists()` to avoid
    // performing blocking syscall on the reactor thread.
    let repo_exists = tokio::fs::metadata(&repo_dir)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false);

    if repo_exists {
        // Pull latest changes with timeout
        tracing::debug!(repo = %skill.repo, "pulling skill repo");
        let pull_result = tokio::time::timeout(
            git_timeout,
            Command::new("git")
                .args(["pull", "--ff-only", "--prune"])
                .current_dir(&repo_dir)
                .output_with_context(),
        )
        .await;

        match pull_result {
            Ok(Ok(output)) if !output.status.success() => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!(repo = %skill.repo, err = %stderr, "git pull failed");
            }
            Ok(Ok(_)) => {
                tracing::debug!(repo = %skill.repo, "skill repo updated");
            }
            Ok(Err(e)) => {
                tracing::warn!(repo = %skill.repo, err = %e, "git pull error");
            }
            Err(_) => {
                tracing::warn!(repo = %skill.repo, "git pull timed out after 60s");
            }
        }
    } else {
        // Clone the repository (shallow for efficiency)
        tracing::debug!(repo = %skill.repo, "cloning skill repo");
        let Some(parent) = repo_dir.parent() else {
            tracing::warn!(repo = %skill.repo, "skill repo path has no parent directory");
            return;
        };
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::warn!(repo = %skill.repo, err = %e, "failed to create parent dir for skill");
            return;
        }
        let Some(repo_dir_str) = repo_dir.to_str() else {
            tracing::warn!(repo = %skill.repo, "skill repo path is not valid UTF-8");
            return;
        };

        let clone_result = tokio::time::timeout(
            git_timeout,
            Command::new("git")
                .args(["clone", "--depth", "1", &repo_url, repo_dir_str])
                .output_with_context(),
        )
        .await;

        match clone_result {
            Ok(Ok(output)) if !output.status.success() => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!(repo = %skill.repo, err = %stderr, "git clone failed");
                // Clean up partial clone to allow retry on next tick
                let _ = tokio::fs::remove_dir_all(&repo_dir).await;
            }
            Ok(Ok(_)) => {
                tracing::info!(repo = %skill.repo, "skill repo cloned");
            }
            Ok(Err(e)) => {
                tracing::warn!(repo = %skill.repo, err = %e, "git clone error");
                let _ = tokio::fs::remove_dir_all(&repo_dir).await;
            }
            Err(_) => {
                tracing::warn!(repo = %skill.repo, "git clone timed out after 60s");
                let _ = tokio::fs::remove_dir_all(&repo_dir).await;
            }
        }
    }
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
    config: &crate::engine::router::RouterConfig,
    store: Option<&Arc<TaskStore>>,
) {
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
        let mut seen_models = std::collections::HashSet::new();
        for comp in COMPLEXITIES {
            if let Some(pool) = config.model_pool_for_complexity(agent, comp) {
                for model in pool {
                    if seen_models.insert(model.clone()) && is_model_in_cooldown(agent, &model) {
                        let key = format!("{agent}:{model}");
                        if matches!(
                            cooldown_reason(&key).as_deref(),
                            Some("persistent_model_error") | Some("model_not_found")
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
        &store,
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
            try_kv_set_prefer_store(&store, "metrics:orch.agents_degraded.alert", "1").await
        {
            tracing::warn!(err = %e, "failed to persist degraded agents alert metric");
        }
    } else if let Err(e) =
        try_kv_set_prefer_store(&store, "metrics:orch.agents_degraded.alert", "0").await
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

    // Edge-triggered: only log when the affected-agents set changes, so a
    // persistent condition doesn't flood the log every sync tick.
    const STALE_AGENTS_KV: &str = "metrics:orch.model_pool_stale_persistent.agents";
    let current_dim = heavily_cooled_agents.join(",");
    let prev_dim = kv_get_prefer_store(&store, STALE_AGENTS_KV)
        .await
        .unwrap_or_default();

    if !heavily_cooled_agents.is_empty() {
        if current_dim != prev_dim {
            tracing::warn!(
                affected_agents = ?heavily_cooled_agents,
                "agent model pool appears stale: persistent model failures in heavily cooled pool"
            );
        }
        if let Err(e) = try_kv_set_prefer_store(&store, STALE_AGENTS_KV, &current_dim).await {
            tracing::warn!(err = %e, "failed to persist stale model pool agents state");
        }
        if let Err(e) = try_kv_set_prefer_store(
            &store,
            "metrics:orch.model_pool_stale_persistent.alert",
            "1",
        )
        .await
        {
            tracing::warn!(err = %e, "failed to persist stale model pool alert metric");
        }
    } else {
        if !prev_dim.is_empty() {
            tracing::info!("agent model pool stale condition cleared");
        }
        if let Err(e) = try_kv_set_prefer_store(&store, STALE_AGENTS_KV, "").await {
            tracing::warn!(err = %e, "failed to clear stale model pool agents state");
        }
        if let Err(e) = try_kv_set_prefer_store(
            &store,
            "metrics:orch.model_pool_stale_persistent.alert",
            "0",
        )
        .await
        {
            tracing::warn!(err = %e, "failed to clear stale model pool alert metric");
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

    let mut existing_by_title: std::collections::HashMap<String, DuplicateTarget> =
        std::collections::HashMap::new();
    let mut accepted_by_title: std::collections::HashMap<String, DuplicateTarget> =
        std::collections::HashMap::new();

    let cutoff = (chrono::Utc::now() - chrono::Duration::hours(24))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    match store.list_for_doctor(repo, &cutoff).await {
        Ok(recent) => {
            let mut insert_target = |task: &crate::store::Task| {
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

    // Track last ingest time for incremental fetches (same pattern as mentions_last_checked).
    // Use 24h fallback on first run or if the key is missing.
    // Note: on startup, the engine clears this key for all repos (see clear_issues_last_ingested)
    // so the first ingest after a restart always uses the 24h window. This prevents issues
    // created during engine downtime from being permanently skipped (GitHub's `since` filters
    // by updated_at, not created_at, so issues created while the engine was down would otherwise
    // be invisible).
    let kv_key = format!("issues_last_ingested:{repo}");
    let fallback = (chrono::Utc::now() - chrono::Duration::hours(24))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let since = store
        .kv_get(&kv_key)
        .await
        .ok()
        .flatten()
        .unwrap_or(fallback);
    let since_for_fetch = since.clone();

    // Fetch all open issues in a single backend call and partition by status label locally.
    // The GitHub backend overrides list_active_open_issues() to use one list_all_open_issues()
    // request instead of a routable call + N per-status calls.
    // Pass `since` to filter issues updated since last ingest for efficiency.
    let all_tasks: Vec<(crate::backends::ExternalTask, Option<Status>)> = match backend
        .list_active_open_issues(Some(&since_for_fetch))
        .await
    {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::debug!(
                ?e,
                "ingest: failed to list active open issues — skipping this cycle"
            );
            return Ok(());
        }
    };

    // Upsert into the store, collecting store IDs for batch status check.
    // Also acknowledge newly detected issues (eyes reaction).
    let mut id_status_pairs: Vec<(i64, crate::backends::Status)> = Vec::new();
    // Collect (store_id, node_id) for newly ingested tasks so we can batch-fetch
    // estimate values from GitHub Projects after the loop.
    let mut new_task_node_ids: Vec<(i64, String)> = Vec::new();
    // Pre-load existing external IDs for this repo to avoid N+1 queries
    // (each get_by_external_id call is an individual SQL query). If the
    // lookup fails, fall back to an empty set so we conservatively treat
    // tasks as new (they will be upserted below).
    let existing_ext_ids: std::collections::HashSet<String> = match store
        .existing_external_ids(repo)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(repo = repo, err = %e, "ingest: failed to load existing external ids — proceeding");
            std::collections::HashSet::new()
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
            if let Some(target) = duplicate_target {
                if target.id != task.id.0 {
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
                            footer = crate::engine::orch_footer()
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
        }

        match store.ensure_external_task(repo, task).await {
            Ok(store_id) => {
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
            if let Some(existing) = existing_map.get(&store_id) {
                if existing.status == TaskStatus::New {
                    let db_status = status_to_task_status(status);
                    if db_status != TaskStatus::New {
                        if let Err(e) = store.update_status(store_id, db_status).await {
                            tracing::debug!(?e, "ingest: status sync failed");
                        }
                    }
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
                    if let Some(&estimate) = estimates.get(&node_id) {
                        if estimate > 0 {
                            if let Err(e) = store.set_estimate_if_zero(store_id, estimate).await {
                                tracing::debug!(
                                    store_id,
                                    estimate,
                                    err = %e,
                                    "ingest: failed to set estimate from project"
                                );
                            }
                        }
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

    // Record last ingest time for next incremental fetch.
    //
    // Advance the cursor to the max `updated_at` actually observed in this fetch, not to
    // wall-clock now(). Using now() creates a watermark race: an issue created or updated
    // between the moment GitHub computed the `since` query results and the moment this line
    // runs (API replication lag, or simply request latency) can be excluded from *this*
    // fetch, and then the cursor jumps past its updated_at anyway. On every subsequent tick
    // `since` is newer than that issue's updated_at, so it is silently skipped forever unless
    // it receives a new comment/label to bump updated_at, or the engine restarts (which clears
    // the cursor, see clear_issues_last_ingested). Deriving the cursor from observed data only
    // is the same safe pattern used by the mentions cursor (see MentionCursor::advance above).
    if let Some(latest) = all_tasks.iter().map(|(t, _)| t.updated_at.as_str()).max() {
        if let Err(e) = store.kv_set(&kv_key, latest).await {
            tracing::error!(key = kv_key, err = %e, "ingest: failed to record last ingest time");
        }
    }

    Ok(())
}

/// Clear `issues_last_ingested:{repo}` for all repos on engine startup.
///
/// This ensures that the first ingest after a restart uses the 24h fallback window
/// instead of an incremental `since` timestamp that may have been set before a period
/// of engine downtime. Without this, issues created while the engine was down would be
/// permanently invisible: GitHub's `since` filter uses `updated_at`, so any issue whose
/// `updated_at` predates the stored timestamp is silently skipped.
pub(crate) async fn clear_issues_last_ingested(store: &crate::store::TaskStore, repo: &str) {
    let kv_key = format!("issues_last_ingested:{repo}");
    match store.kv_delete(&kv_key).await {
        Ok(_) => tracing::info!(
            repo,
            "cleared issues_last_ingested for full re-scan on startup"
        ),
        Err(e) => tracing::warn!(repo, err = %e, "failed to clear issues_last_ingested on startup"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{ExternalId, ExternalTask, Mention, Status};
    use crate::engine::cooldown::set_agent_cooldown;
    use crate::store::{RunTokenUsage, TaskStore};
    use async_trait::async_trait;
    use serial_test::serial;
    use std::sync::Arc;

    // ── ingest_external_tasks tests ─────────────────────────────────────────

    /// Mock backend that returns configurable tasks per status.
    struct IngestMockBackend {
        /// Stored as (status_label, tasks) pairs since Status doesn't impl Hash.
        tasks: Vec<(String, Vec<ExternalTask>)>,
        dedup_result: bool,
        comments: tokio::sync::Mutex<Vec<(String, String)>>,
        status_updates: tokio::sync::Mutex<Vec<(String, Status)>>,
        project_syncs: tokio::sync::Mutex<Vec<(String, Status)>>,
    }

    impl IngestMockBackend {
        fn with_tasks(tasks: Vec<(Status, ExternalTask)>) -> Arc<Self> {
            Self::with_tasks_and_dedup(tasks, false)
        }

        fn with_tasks_and_dedup(
            tasks: Vec<(Status, ExternalTask)>,
            dedup_result: bool,
        ) -> Arc<Self> {
            let mut grouped: Vec<(String, Vec<ExternalTask>)> = Vec::new();
            for (status, task) in tasks {
                let label = status.as_label().to_string();
                if let Some(entry) = grouped.iter_mut().find(|(l, _)| l == &label) {
                    entry.1.push(task);
                } else {
                    grouped.push((label, vec![task]));
                }
            }
            Arc::new(Self {
                tasks: grouped,
                dedup_result,
                comments: tokio::sync::Mutex::new(Vec::new()),
                status_updates: tokio::sync::Mutex::new(Vec::new()),
                project_syncs: tokio::sync::Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl ExternalBackend for IngestMockBackend {
        fn name(&self) -> &str {
            "ingest-mock"
        }
        async fn create_task(&self, _: &str, _: &str, _: &[String]) -> anyhow::Result<ExternalId> {
            Ok(ExternalId("new".into()))
        }
        async fn get_task(&self, _: &ExternalId) -> anyhow::Result<ExternalTask> {
            anyhow::bail!("not implemented")
        }
        async fn list_by_status(&self, status: Status) -> anyhow::Result<Vec<ExternalTask>> {
            let label = status.as_label().to_string();
            Ok(self
                .tasks
                .iter()
                .find(|(l, _)| l == &label)
                .map(|(_, t)| t.clone())
                .unwrap_or_default())
        }
        async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
            Ok(vec![])
        }
        async fn list_active_open_issues(
            &self,
            _: Option<&str>,
        ) -> anyhow::Result<Vec<(ExternalTask, Option<Status>)>> {
            let mut result = Vec::new();
            for (label, tasks) in &self.tasks {
                let status = match label.as_str() {
                    "status:new" => None,
                    "status:routed" => Some(Status::Routed),
                    "status:in_progress" => Some(Status::InProgress),
                    "status:needs_review" => Some(Status::NeedsReview),
                    "status:in_review" => Some(Status::InReview),
                    "status:blocked" => Some(Status::Blocked),
                    _ => None,
                };
                for task in tasks {
                    result.push((task.clone(), status));
                }
            }
            Ok(result)
        }
        async fn post_comment(&self, id: &ExternalId, body: &str) -> anyhow::Result<()> {
            self.comments
                .lock()
                .await
                .push((id.0.clone(), body.to_string()));
            Ok(())
        }
        async fn set_labels(&self, _: &ExternalId, _: &[String]) -> anyhow::Result<()> {
            Ok(())
        }
        async fn remove_label(&self, _: &ExternalId, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn get_sub_issues(&self, _: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
            Ok(vec![])
        }
        async fn health_check(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn get_mentions(&self, _: &str) -> anyhow::Result<Vec<Mention>> {
            Ok(vec![])
        }
        async fn update_status(&self, id: &ExternalId, status: Status) -> anyhow::Result<()> {
            self.status_updates
                .lock()
                .await
                .push((id.0.clone(), status));
            Ok(())
        }

        async fn has_open_issue_with_title(&self, _: &str, _: &str) -> anyhow::Result<bool> {
            Ok(self.dedup_result)
        }

        async fn sync_to_project(
            &self,
            id: &ExternalId,
            status: Status,
        ) -> anyhow::Result<Option<String>> {
            self.project_syncs.lock().await.push((id.0.clone(), status));
            Ok(None) // Mock doesn't have real node IDs.
        }
    }

    fn make_ext_task(id: &str, title: &str) -> ExternalTask {
        ExternalTask {
            id: ExternalId(id.to_string()),
            title: title.to_string(),
            body: "".to_string(),
            state: "open".to_string(),
            labels: vec![],
            author: "user".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: format!("https://github.com/owner/repo/issues/{id}"),
        }
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn ingest_upserts_tasks_into_store() {
        crate::engine::cooldown::reset_global_state().await;
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![
            (Status::New, make_ext_task("1", "First issue")),
            (Status::InProgress, make_ext_task("2", "Second issue")),
        ]);
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        ingest_external_tasks(&backend, "owner/repo", &store)
            .await
            .unwrap();

        // Both tasks should be in the store
        let all = store.list_all("owner/repo").await.unwrap();
        assert_eq!(all.len(), 2);

        let task1 = store
            .get_by_external_id("owner/repo", "1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(task1.title, "First issue");
        assert_eq!(task1.status, TaskStatus::New);

        let task2 = store
            .get_by_external_id("owner/repo", "2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(task2.title, "Second issue");
        assert_eq!(task2.status, crate::store::TaskStatus::InProgress);
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn ingest_advances_cursor_to_max_observed_updated_at_not_wall_clock() {
        // Regression test for the watermark race: the `issues_last_ingested` cursor must be
        // derived from the max `updated_at` actually seen in the fetch, not from
        // chrono::Utc::now(). Otherwise an issue whose updated_at lags real time (e.g. GitHub
        // API replication lag between the `since` query and this line running) gets skipped by
        // every subsequent `since` fetch forever, even though it was never actually ingested.
        crate::engine::cooldown::reset_global_state().await;
        let mut task = make_ext_task("1", "First issue");
        task.updated_at = "2026-01-01T00:00:00Z".to_string();
        let backend: Arc<dyn ExternalBackend> =
            IngestMockBackend::with_tasks(vec![(Status::New, task)]);
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        ingest_external_tasks(&backend, "owner/repo", &store)
            .await
            .unwrap();

        let cursor = store
            .kv_get("issues_last_ingested:owner/repo")
            .await
            .unwrap();
        assert_eq!(
            cursor,
            Some("2026-01-01T00:00:00Z".to_string()),
            "cursor must match the max updated_at observed in the fetch, not wall-clock now()"
        );
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn ingest_updates_existing_tasks() {
        crate::engine::cooldown::reset_global_state().await;
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![(
            Status::Routed,
            make_ext_task("42", "Updated title"),
        )]);
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        // Pre-create the task
        store
            .create(&crate::store::NewTask {
                external_id: Some("42".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Original title".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        ingest_external_tasks(&backend, "owner/repo", &store)
            .await
            .unwrap();

        // Should have updated the title
        let task = store
            .get_by_external_id("owner/repo", "42")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(task.title, "Updated title");
        assert_eq!(task.status, crate::store::TaskStatus::Routed);
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn ingest_closes_duplicate_titles() {
        crate::engine::cooldown::reset_global_state().await;
        use crate::store::{NewTask, TaskStore};

        let backend = IngestMockBackend::with_tasks_and_dedup(
            vec![(Status::New, make_ext_task("2", "Duplicate issue"))],
            true,
        );
        let backend_dyn: Arc<dyn ExternalBackend> = backend.clone();
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        store
            .create(&NewTask {
                external_id: Some("1".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Duplicate issue".to_string(),
                labels: vec!["status:new".to_string()],
                ..Default::default()
            })
            .await
            .unwrap();

        ingest_external_tasks(&backend_dyn, "owner/repo", &store)
            .await
            .unwrap();

        let all = store.list_all("owner/repo").await.unwrap();
        assert_eq!(all.len(), 1);

        let comments = backend.comments.lock().await;
        assert_eq!(comments.len(), 1);
        assert!(comments[0].1.contains("Duplicate of #1"));

        let status_updates = backend.status_updates.lock().await;
        assert_eq!(status_updates.len(), 1);
        assert_eq!(status_updates[0].0, "2");
        assert_eq!(status_updates[0].1, Status::Done);
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn ingest_handles_empty_backend() {
        crate::engine::cooldown::reset_global_state().await;
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        // Should not error on empty backend
        let result = ingest_external_tasks(&backend, "owner/repo", &store).await;
        assert!(result.is_ok());

        let all = store.list_all("owner/repo").await.unwrap();
        assert!(all.is_empty());
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn ingest_syncs_status_correctly_across_statuses() {
        crate::engine::cooldown::reset_global_state().await;
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![
            (Status::New, make_ext_task("1", "New")),
            (Status::Routed, make_ext_task("2", "Routed")),
            (Status::InProgress, make_ext_task("3", "InProgress")),
            (Status::NeedsReview, make_ext_task("4", "NeedsReview")),
            (Status::InReview, make_ext_task("5", "InReview")),
            (Status::Blocked, make_ext_task("6", "Blocked")),
        ]);
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        ingest_external_tasks(&backend, "owner/repo", &store)
            .await
            .unwrap();

        let counts = store.status_counts("owner/repo").await.unwrap();
        assert_eq!(counts.get("new"), Some(&1));
        assert_eq!(counts.get("routed"), Some(&1));
        assert_eq!(counts.get("in_progress"), Some(&1));
        assert_eq!(counts.get("needs_review"), Some(&1));
        assert_eq!(counts.get("in_review"), Some(&1));
        assert_eq!(counts.get("blocked"), Some(&1));
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn ingest_does_not_overwrite_store_authoritative_status() {
        crate::engine::cooldown::reset_global_state().await;
        // Backend reports task as New (GitHub labels haven't caught up yet)
        let backend: Arc<dyn ExternalBackend> =
            IngestMockBackend::with_tasks(vec![(Status::New, make_ext_task("99", "My task"))]);
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        // Pre-create the task and advance it to Routed in the store
        let id = store
            .create(&crate::store::NewTask {
                external_id: Some("99".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "My task".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::Routed)
            .await
            .unwrap();

        // Ingest should NOT overwrite Routed back to New
        ingest_external_tasks(&backend, "owner/repo", &store)
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(
            task.status,
            crate::store::TaskStatus::Routed,
            "ingest must not overwrite store-authoritative status"
        );
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn ingest_sync_to_project_called_only_for_new_tasks() {
        crate::engine::cooldown::reset_global_state().await;
        use crate::store::{NewTask, TaskStore};

        // Two tasks: "10" is pre-existing, "11" is new.
        let backend = IngestMockBackend::with_tasks(vec![
            (Status::New, make_ext_task("10", "Existing task")),
            (Status::New, make_ext_task("11", "New task")),
        ]);
        let backend_dyn: Arc<dyn ExternalBackend> = backend.clone();
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        // Pre-insert task "10" so it already exists in the store.
        store
            .create(&NewTask {
                external_id: Some("10".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Existing task".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        ingest_external_tasks(&backend_dyn, "owner/repo", &store)
            .await
            .unwrap();

        let syncs = backend.project_syncs.lock().await;
        // Only the new task "11" should trigger sync_to_project — not the pre-existing "10".
        assert_eq!(
            syncs.len(),
            1,
            "sync_to_project must be called exactly once (for new task only), got: {syncs:?}"
        );
        assert_eq!(syncs[0].0, "11", "sync_to_project must target the new task");
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn ingest_handles_mixed_labeled_and_unlabeled_open_issues() {
        crate::engine::cooldown::reset_global_state().await;
        // Simulate a backend where some issues have no status label (routable/unlabeled)
        // and others carry active status labels.  Both kinds must be ingested and the
        // store status must reflect the label where present.
        struct MixedBackend;

        #[async_trait]
        impl ExternalBackend for MixedBackend {
            fn name(&self) -> &str {
                "mixed"
            }
            async fn create_task(
                &self,
                _: &str,
                _: &str,
                _: &[String],
            ) -> anyhow::Result<ExternalId> {
                Ok(ExternalId("x".into()))
            }
            async fn get_task(&self, _: &ExternalId) -> anyhow::Result<ExternalTask> {
                anyhow::bail!("not implemented")
            }
            async fn list_by_status(&self, _: Status) -> anyhow::Result<Vec<ExternalTask>> {
                Ok(vec![])
            }
            /// Return a mix: one unlabeled (no status) + one labeled in_progress.
            async fn list_active_open_issues(
                &self,
                _: Option<&str>,
            ) -> anyhow::Result<Vec<(ExternalTask, Option<Status>)>> {
                Ok(vec![
                    (make_ext_task("100", "Unlabeled routable"), None),
                    (
                        make_ext_task("101", "In progress issue"),
                        Some(Status::InProgress),
                    ),
                ])
            }
            async fn post_comment(&self, _: &ExternalId, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            async fn set_labels(&self, _: &ExternalId, _: &[String]) -> anyhow::Result<()> {
                Ok(())
            }
            async fn remove_label(&self, _: &ExternalId, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            async fn get_sub_issues(&self, _: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
                Ok(vec![])
            }
            async fn health_check(&self) -> anyhow::Result<()> {
                Ok(())
            }
            async fn get_mentions(&self, _: &str) -> anyhow::Result<Vec<Mention>> {
                Ok(vec![])
            }
            async fn update_status(&self, _: &ExternalId, _: Status) -> anyhow::Result<()> {
                Ok(())
            }
            async fn has_open_issue_with_title(&self, _: &str, _: &str) -> anyhow::Result<bool> {
                Ok(false)
            }
            async fn sync_to_project(
                &self,
                _: &ExternalId,
                _: Status,
            ) -> anyhow::Result<Option<String>> {
                Ok(None)
            }
        }

        let backend: Arc<dyn ExternalBackend> = Arc::new(MixedBackend);
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        ingest_external_tasks(&backend, "owner/repo", &store)
            .await
            .unwrap();

        let all = store.list_all("owner/repo").await.unwrap();
        assert_eq!(all.len(), 2, "both issues must be ingested");

        let unlabeled = store
            .get_by_external_id("owner/repo", "100")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            unlabeled.status,
            crate::store::TaskStatus::New,
            "unlabeled issue should start as New"
        );

        let in_progress = store
            .get_by_external_id("owner/repo", "101")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            in_progress.status,
            crate::store::TaskStatus::InProgress,
            "labeled in_progress issue must be synced to InProgress"
        );
    }

    // ── kv_get_prefer_store / kv_set_prefer_store ────────────────────

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn kv_get_prefer_store_reads_from_store() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        store.kv_set("k1", "store_val").await.unwrap();

        let opt = Some(&store);
        let val = kv_get_prefer_store(&opt, "k1").await;
        assert_eq!(val.as_deref(), Some("store_val"));
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn kv_get_prefer_store_returns_none_without_store() {
        crate::engine::cooldown::reset_global_state().await;
        let opt: Option<&Arc<TaskStore>> = None;
        let val = kv_get_prefer_store(&opt, "k2").await;
        assert_eq!(val, None);
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn kv_set_prefer_store_writes_to_store() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        let opt = Some(&store);
        kv_set_prefer_store(&opt, "k3", "val3").await;

        assert_eq!(store.kv_get("k3").await.unwrap().as_deref(), Some("val3"));
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn kv_set_prefer_store_noop_without_store() {
        crate::engine::cooldown::reset_global_state().await;
        let opt: Option<&Arc<TaskStore>> = None;
        // Should not panic
        kv_set_prefer_store(&opt, "k4", "val4").await;
    }

    // ── try_kv_get_prefer_store / try_kv_set_prefer_store ────────────────

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn try_kv_get_prefer_store_returns_value_from_store() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        store.kv_set("try_k1", "try_val1").await.unwrap();

        let opt = Some(&store);
        let result = try_kv_get_prefer_store(&opt, "try_k1").await;
        assert_eq!(result.unwrap().as_deref(), Some("try_val1"));
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn try_kv_get_prefer_store_returns_none_for_missing_key() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let opt = Some(&store);

        let result = try_kv_get_prefer_store(&opt, "nonexistent_key").await;
        assert_eq!(result.unwrap(), None); // Key doesn't exist, but query succeeded
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn try_kv_get_prefer_store_returns_none_without_store() {
        crate::engine::cooldown::reset_global_state().await;
        let opt: Option<&Arc<TaskStore>> = None;
        let result = try_kv_get_prefer_store(&opt, "try_k2").await;
        assert_eq!(result.unwrap(), None);
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn try_kv_set_prefer_store_writes_to_store() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let opt = Some(&store);

        let result = try_kv_set_prefer_store(&opt, "try_k3", "try_val3").await;
        assert!(result.is_ok());
        assert_eq!(
            store.kv_get("try_k3").await.unwrap().as_deref(),
            Some("try_val3")
        );
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn try_kv_set_prefer_store_ok_without_store() {
        crate::engine::cooldown::reset_global_state().await;
        let opt: Option<&Arc<TaskStore>> = None;
        // Should not error - best effort when store unavailable
        let result = try_kv_set_prefer_store(&opt, "try_k4", "try_val4").await;
        assert!(result.is_ok());
    }

    // ── dispatching lock tests ──────────────────────────────────────────

    #[serial(cooldown_state)]
    #[test]
    fn dispatching_set_blocks_duplicate_processing() {
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

        let repo = "owner/repo";
        let task_id = "42";
        let dispatch_key = format!("{}/{}", repo, task_id);

        // Initially not in the map
        assert!(!dispatching.contains_key(&dispatch_key));

        // Insert — simulates dispatch starting
        dispatching.insert(dispatch_key.clone(), task_id.to_string());

        // Now should be blocked
        assert!(
            dispatching.contains_key(&dispatch_key),
            "task should be locked while dispatching"
        );

        // Remove — simulates review completion
        dispatching.remove(&dispatch_key);

        // Now should be free again
        assert!(
            !dispatching.contains_key(&dispatch_key),
            "task should be unlocked after review completes"
        );
    }

    #[serial(cooldown_state)]
    #[test]
    fn dispatching_set_does_not_block_other_tasks() {
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

        let repo = "owner/repo";

        // Lock task 42
        let key_42 = format!("{}/42", repo);
        dispatching.insert(key_42.clone(), "42".to_string());

        // Task 43 should NOT be blocked
        let key_43 = format!("{}/43", repo);
        assert!(
            dispatching.contains_key(&key_42),
            "task 42 should be locked"
        );
        assert!(
            !dispatching.contains_key(&key_43),
            "task 43 should not be locked"
        );
    }

    /// Regression test for a6d8b9a.
    ///
    /// Before the fix, `sync_tick` step 5 transitioned a task to `InReview` and then
    /// spawned a `tokio::spawn` review agent WITHOUT first inserting the `dispatch_key`
    /// into the dispatching set. This created a window where `review_open_prs` (step 4)
    /// in a concurrent sync tick could see the task as `InReview`, find the key absent,
    /// and re-dispatch it — silently discarding human `CHANGES_REQUESTED` feedback when
    /// the review agent later completed and marked the task `Done`.
    ///
    /// The fix: insert `dispatch_key` BEFORE `tokio::spawn`, remove it AFTER the future
    /// completes. This test verifies that invariant using tokio channels to synchronize
    /// with a simulated concurrent caller (review_open_prs).
    #[serial(cooldown_state)]
    #[tokio::test]
    async fn dispatch_key_held_during_review_agent_execution() {
        crate::engine::cooldown::reset_global_state().await;
        use tokio::sync::oneshot;

        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

        let dispatch_key = "owner/repo/42".to_string();

        // Step 1: insert before spawning — this is the invariant added by a6d8b9a.
        dispatching.insert(dispatch_key.clone(), "42".to_string());

        let (agent_started_tx, agent_started_rx) = oneshot::channel::<()>();
        let (check_done_tx, check_done_rx) = oneshot::channel::<()>();

        let dispatching_agent = Arc::clone(&dispatching);
        let key_agent = dispatch_key.clone();

        // Step 2: spawn review agent — key is ALREADY in the set (fix invariant holds).
        let review_task = tokio::spawn(async move {
            agent_started_tx.send(()).unwrap();
            // Simulate review agent work — hold until the concurrent check is done.
            check_done_rx.await.unwrap();
            // Remove from dispatching set on completion (mirror of tick.rs/sync.rs).
            dispatching_agent.remove(&key_agent);
        });

        // Step 3: concurrent review_open_prs check — simulates step 4 of another
        // sync_tick invocation running while the review agent is still active.
        agent_started_rx.await.unwrap();
        assert!(
            dispatching.contains_key(&dispatch_key),
            "dispatch_key must be visible in the dispatching set while the review \
             agent is running — without the a6d8b9a fix, review_open_prs would see \
             the key absent and re-dispatch the task, silently dropping \
             CHANGES_REQUESTED feedback"
        );

        // Step 4: review agent completes.
        check_done_tx.send(()).unwrap();
        review_task.await.unwrap();

        // Step 5: key released — review_open_prs can now act on the task if needed.
        assert!(
            !dispatching.contains_key(&dispatch_key),
            "dispatch_key must be removed from the dispatching set after the \
             review agent completes so subsequent sync ticks can process the task"
        );
    }

    /// Regression test for issue #833.
    ///
    /// Before the fix, `dispatch_key` was inserted into the `dispatching` HashSet
    /// *outside* the spawned task but removed *inside* it.  When the spawned async
    /// block panicked (e.g. an `unwrap()` inside `review_and_merge`), Tokio caught
    /// the panic and terminated the task without running the cleanup code, leaving
    /// the key in the set permanently.  Subsequent stuck-task recovery would reset
    /// the task to `NeedsReview`, but the subscriber/sync would see the key still
    /// present and skip it forever — a permanent review loop until service restart.
    ///
    /// The fix introduces `DispatchGuard`, a RAII wrapper whose `Drop` impl removes
    /// the key unconditionally, even when the task unwinds via panic.
    #[serial(cooldown_state)]
    #[tokio::test]
    async fn dispatch_guard_releases_key_on_panic() {
        crate::engine::cooldown::reset_global_state().await;
        use crate::engine::dispatch_guard::DispatchGuard;

        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        let key = "owner/repo/42".to_string();

        // Insert key before spawn — mirrors the production invariant established
        // by the a6d8b9a fix (key must be visible before the spawn so concurrent
        // review_open_prs callers see it and skip the task).
        dispatching.insert(key.clone(), "42".to_string());

        // Create the guard (takes ownership of the removal obligation).
        let guard = DispatchGuard::new(Arc::clone(&dispatching), key.clone());

        // Spawn a task that panics before the normal completion path.
        let handle = tokio::spawn(async move {
            let _guard = guard; // moved in; Drop removes key whether we panic or not
            panic!("simulated panic inside review_and_merge");
        });

        // Absorb the JoinError — the panic is expected.
        let _ = handle.await;

        // Key MUST be gone even though the task panicked.
        assert!(
            !dispatching.contains_key(&key),
            "dispatch key must be removed from the dispatching set even when the \
             spawned review task panics — without DispatchGuard the key leaks and \
             the task gets stuck in a permanent review loop"
        );
    }

    /// An InReview task with no tmux session should be reset to NeedsReview
    /// regardless of review_session_expected — the review agent is dead.
    #[serial(cooldown_state)]
    #[tokio::test]
    async fn stale_in_review_recovery_resets_orphaned_task_without_session() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let router = Arc::new(RwLock::new(crate::engine::router::Router::from_config()));
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        let auto_merge_in_flight: Arc<DashSet<String>> = Arc::new(DashSet::new());

        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: "55",
                title: "Orphaned review",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::InReview)
            .await
            .unwrap();
        store
            .set_fields(id, &[("branch", serde_json::json!("feature/55"))])
            .await
            .unwrap();
        // Make it old enough to be considered stale (>2 min)
        sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
            .bind(id)
            .execute(store.pool())
            .await
            .unwrap();

        sync_tick(
            &backend,
            &tmux,
            "owner/repo",
            &EngineConfig::default(),
            &router,
            &task_manager,
            &store,
            &dispatching,
            &auto_merge_in_flight,
        )
        .await
        .unwrap();

        // Should be reset to NeedsReview since no tmux session exists
        let task = store.get(id).await.unwrap();
        assert_eq!(task.status, crate::store::TaskStatus::NeedsReview);
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn needs_review_refire_increments_and_fires_when_old_enough() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let router = Arc::new(RwLock::new(crate::engine::router::Router::from_config()));
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        let auto_merge_in_flight: Arc<DashSet<String>> = Arc::new(DashSet::new());

        // Create external task and set to NeedsReview
        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: "201",
                title: "Needs review old",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::NeedsReview)
            .await
            .unwrap();
        // Backdate updated_at so it's considered stale (>1 minute)
        sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
            .bind(id)
            .execute(store.pool())
            .await
            .unwrap();

        // Ensure initial counter is zero
        let t = store.get(id).await.unwrap();
        assert_eq!(t.needs_review_refires, 0);

        // Run sync tick — should increment counter and re-fire (no status transition beyond NeedsReview)
        sync_tick(
            &backend,
            &tmux,
            "owner/repo",
            &EngineConfig::default(),
            &router,
            &task_manager,
            &store,
            &dispatching,
            &auto_merge_in_flight,
        )
        .await
        .unwrap();

        let t2 = store.get(id).await.unwrap();
        assert_eq!(t2.needs_review_refires, 1);
        assert_eq!(t2.status, crate::store::TaskStatus::NeedsReview);
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn needs_review_backoff_delays_fire_but_increments_counter() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let router = Arc::new(RwLock::new(crate::engine::router::Router::from_config()));
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        let auto_merge_in_flight: Arc<DashSet<String>> = Arc::new(DashSet::new());

        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: "202",
                title: "Needs review backoff",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::NeedsReview)
            .await
            .unwrap();

        // Set existing refires to 1 to simulate a prior attempt
        store
            .set_fields(id, &[("needs_review_refires", serde_json::json!(1))])
            .await
            .unwrap();

        // Set updated_at to now - 1 minute (>= MIN but less than required for refires=2 which is 2 minutes)
        let now_minus_1 = (chrono::Utc::now() - chrono::Duration::minutes(1))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        sqlx::query("UPDATE tasks SET updated_at = ? WHERE id = ?")
            .bind(now_minus_1.clone())
            .bind(id)
            .execute(store.pool())
            .await
            .unwrap();

        let before = store.get(id).await.unwrap();
        assert_eq!(before.needs_review_refires, 1);

        sync_tick(
            &backend,
            &tmux,
            "owner/repo",
            &EngineConfig::default(),
            &router,
            &task_manager,
            &store,
            &dispatching,
            &auto_merge_in_flight,
        )
        .await
        .unwrap();

        let after = store.get(id).await.unwrap();
        // Counter should NOT have been incremented on a mere backoff delay —
        // only actual re-fire attempts increment the counter.
        assert_eq!(after.needs_review_refires, 1);
        assert_eq!(after.updated_at, now_minus_1);
        assert_eq!(after.status, crate::store::TaskStatus::NeedsReview);
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn needs_review_escalates_to_blocked_after_max_refires() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let router = Arc::new(RwLock::new(crate::engine::router::Router::from_config()));
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        let auto_merge_in_flight: Arc<DashSet<String>> = Arc::new(DashSet::new());

        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: "203",
                title: "Needs review escalate",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::NeedsReview)
            .await
            .unwrap();

        // Set counter to MAX (5) so incrementing to MAX+1 (6) triggers escalation.
        // With the fix (new_refires > MAX), escalation happens at the 6th attempt
        // (current=5 -> new=6), allowing exactly 5 refires before blocking.
        store
            .set_fields(id, &[("needs_review_refires", serde_json::json!(5))])
            .await
            .unwrap();
        // Backdate updated_at so age check passes
        sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
            .bind(id)
            .execute(store.pool())
            .await
            .unwrap();

        sync_tick(
            &backend,
            &tmux,
            "owner/repo",
            &EngineConfig::default(),
            &router,
            &task_manager,
            &store,
            &dispatching,
            &auto_merge_in_flight,
        )
        .await
        .unwrap();

        let after = store.get(id).await.unwrap();
        assert_eq!(after.status, crate::store::TaskStatus::Blocked);
        assert!(after.block_reason.is_some());
    }

    /// Verify that exactly MAX_NEEDS_REVIEW_REFIRE_ATTEMPTS refires are allowed
    /// before escalation. Task should still be NeedsReview at 5 refires,
    /// only block at the 6th attempt.
    #[serial(cooldown_state)]
    #[tokio::test]
    async fn needs_review_allows_max_refires_before_escalation() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let router = Arc::new(RwLock::new(crate::engine::router::Router::from_config()));
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        let auto_merge_in_flight: Arc<DashSet<String>> = Arc::new(DashSet::new());

        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: "204",
                title: "Max refires test",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::NeedsReview)
            .await
            .unwrap();

        // Set counter to MAX-1 (4) so incrementing hits MAX (5) but does NOT escalate.
        // With new_refires > MAX, escalation happens at 6, so 5 should still be safe.
        store
            .set_fields(id, &[("needs_review_refires", serde_json::json!(4))])
            .await
            .unwrap();
        // Backdate updated_at so age check passes
        sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
            .bind(id)
            .execute(store.pool())
            .await
            .unwrap();

        sync_tick(
            &backend,
            &tmux,
            "owner/repo",
            &EngineConfig::default(),
            &router,
            &task_manager,
            &store,
            &dispatching,
            &auto_merge_in_flight,
        )
        .await
        .unwrap();

        let after = store.get(id).await.unwrap();
        // Task should still be NeedsReview at 5 refires (MAX), not Blocked
        assert_eq!(after.status, crate::store::TaskStatus::NeedsReview);
        assert_eq!(after.needs_review_refires, 5);
        assert!(after.block_reason.is_none());
    }

    // ── refire_and_escalate_stale_needs_review_global ──────────────────────

    fn router_with_healthy_review_agent() -> crate::engine::router::Router {
        use crate::engine::router::{Router, RouterConfig};
        let mut config = RouterConfig::default();
        config
            .model_map
            .entry("review".to_string())
            .or_default()
            .insert("claude".to_string(), vec!["sonnet".to_string()]);
        let mut router = Router::new(config);
        router.available_agents = vec!["claude".to_string()];
        router
    }

    /// Regression test for #3469: the per-repo refire/escalation logic inside
    /// `sync_tick` never runs for a repo that has been removed from the active
    /// `projects:` list, so a task stuck in NeedsReview for such a repo never
    /// refires. The global sweep must catch it via `list_all_by_status_global`
    /// and write through the store directly, bypassing the repo-scoped
    /// `TaskManager` (which is scoped to a *different*, currently-active repo).
    #[serial(cooldown_state)]
    #[tokio::test]
    async fn global_refire_sweep_refires_stale_task_from_inactive_repo() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        // TaskManager scoped to the active repo — NOT the repo the task belongs to.
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            store.clone(),
            "active/repo".to_string(),
        ));
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        let router = router_with_healthy_review_agent();

        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "inactive/repo",
                ext_id: "490",
                title: "Stale needs_review from inactive repo",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::NeedsReview)
            .await
            .unwrap();
        sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
            .bind(id)
            .execute(store.pool())
            .await
            .unwrap();

        refire_and_escalate_stale_needs_review_global(&task_manager, &store, &router, &dispatching)
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(
            task.needs_review_refires, 1,
            "cross-repo task must be refired"
        );
        assert_eq!(task.status, crate::store::TaskStatus::NeedsReview);
    }

    /// Companion to the refire test: after `MAX_NEEDS_REVIEW_REFIRE_ATTEMPTS` refires,
    /// a cross-repo task must escalate to Blocked, same as the per-repo `sync_tick` path.
    #[serial(cooldown_state)]
    #[tokio::test]
    async fn global_refire_sweep_escalates_cross_repo_task_to_blocked() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            store.clone(),
            "active/repo".to_string(),
        ));
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        let router = router_with_healthy_review_agent();

        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "inactive/repo",
                ext_id: "493",
                title: "Exhausted refires from inactive repo",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::NeedsReview)
            .await
            .unwrap();
        store
            .set_fields(id, &[("needs_review_refires", serde_json::json!(5))])
            .await
            .unwrap();
        sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
            .bind(id)
            .execute(store.pool())
            .await
            .unwrap();

        refire_and_escalate_stale_needs_review_global(&task_manager, &store, &router, &dispatching)
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(task.status, crate::store::TaskStatus::Blocked);
        assert_eq!(
            task.block_reason.as_deref(),
            Some("review agent rebroadcast escalated after repeated retries")
        );
    }

    /// When every review agent is cooled, the global sweep must skip entirely —
    /// re-firing while all agents are cooled would waste the refire counter and
    /// could escalate a task to Blocked purely because agents happened to be
    /// temporarily unavailable, mirroring `sync_tick`'s per-repo skip.
    #[serial(cooldown_state)]
    #[tokio::test]
    async fn global_refire_sweep_skips_when_all_review_agents_cooled() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            store.clone(),
            "active/repo".to_string(),
        ));
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        // RouterConfig::default() has no "review" model map entries, so the one
        // available agent has no healthy model for "review" — all cooled.
        let router = crate::engine::router::Router::new_for_test(
            crate::engine::router::RouterConfig::default(),
            vec!["claude".to_string()],
        );

        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "inactive/repo",
                ext_id: "494",
                title: "Stale needs_review while agents cooled",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::NeedsReview)
            .await
            .unwrap();
        sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
            .bind(id)
            .execute(store.pool())
            .await
            .unwrap();

        refire_and_escalate_stale_needs_review_global(&task_manager, &store, &router, &dispatching)
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(
            task.needs_review_refires, 0,
            "counter must not be touched while cooled"
        );
        assert_eq!(task.status, crate::store::TaskStatus::NeedsReview);
    }

    /// Regression test for #3499: a task blocked with the rebroadcast-escalation
    /// reason after surviving one review round (`review_cycles > 0`) must be
    /// recovered back to NeedsReview once review agents are routable again.
    /// Previously the `review_cycles != 0` filter excluded these tasks, stranding
    /// them permanently even when agents recovered.
    #[serial(cooldown_state)]
    #[tokio::test]
    async fn auto_recover_rebroadcast_blocked_task_after_review_cycle() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            store.clone(),
            "owner/repo".to_string(),
        ));
        let router = Arc::new(RwLock::new(router_with_healthy_review_agent()));

        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: "511",
                title: "Rebroadcast blocked after review",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::Blocked)
            .await
            .unwrap();
        // Mirror the state left by the refire-exhaustion escalation path.
        store
            .set_block_reason(
                id,
                Some("review agent rebroadcast escalated after repeated retries"),
            )
            .await
            .unwrap();
        store
            .set_fields(id, &[("review_cycles", serde_json::json!(1))])
            .await
            .unwrap();
        store
            .set_fields(id, &[("needs_review_refires", serde_json::json!(6))])
            .await
            .unwrap();
        // Backdate so the MIN_BLOCK_AGE_MINUTES (>=5 min) age filter passes.
        sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
            .bind(id)
            .execute(store.pool())
            .await
            .unwrap();

        auto_recover_rebroadcast_blocked_tasks("owner/repo", &router, &task_manager, &store)
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(
            task.status,
            crate::store::TaskStatus::NeedsReview,
            "blocked rebroadcast task must be recovered to needs_review"
        );
        assert!(
            task.block_reason.is_none(),
            "block_reason must be cleared on recovery"
        );
        assert_eq!(
            task.needs_review_refires, 0,
            "needs_review_refires must be reset to 0 on recovery"
        );
    }

    /// A fresh InReview task (<1 min old) should NOT be reset — the review
    /// agent may still be starting up.
    #[serial(cooldown_state)]
    #[tokio::test]
    async fn stale_in_review_recovery_skips_fresh_tasks() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let router = Arc::new(RwLock::new(crate::engine::router::Router::from_config()));
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        let auto_merge_in_flight: Arc<DashSet<String>> = Arc::new(DashSet::new());

        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: "66",
                title: "Fresh review",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::InReview)
            .await
            .unwrap();
        store
            .set_fields(id, &[("branch", serde_json::json!("feature/66"))])
            .await
            .unwrap();
        // Don't backdate — task is fresh (just created)

        sync_tick(
            &backend,
            &tmux,
            "owner/repo",
            &EngineConfig::default(),
            &router,
            &task_manager,
            &store,
            &dispatching,
            &auto_merge_in_flight,
        )
        .await
        .unwrap();

        // Should stay InReview — too young to be considered orphaned
        let task = store.get(id).await.unwrap();
        assert_eq!(task.status, crate::store::TaskStatus::InReview);
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn auto_unblock_routes_recoverable_failure() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            store.clone(),
            "owner/repo".to_string(),
        ));
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: "77",
                title: "Blocked task",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::Blocked)
            .await
            .unwrap();
        store
            .set_fields(
                id,
                &[
                    ("agent", serde_json::json!("codex")),
                    ("model", serde_json::json!("gpt-5")),
                ],
            )
            .await
            .unwrap();

        let run_id = store
            .start_run(&crate::store::StartRun {
                task_id: id,
                attempt: 1,
                run_type: "agent",
                agent: "codex",
                model: "gpt-5",
                command: "codex --model gpt-5",
                prompt: "do thing",
            })
            .await
            .unwrap();
        store
            .complete_run(&crate::store::CompleteRun {
                run_id,
                exit_code: Some(1),
                stdout: "",
                stderr: "",
                parsed: "",
                outcome: "rate_limit",
                error: "rate limit exceeded",
                tokens: RunTokenUsage::default(),
            })
            .await
            .unwrap();

        auto_unblock_blocked_tasks("owner/repo", &task_manager, &store, &dispatching)
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(task.status, TaskStatus::New);
        assert!(task.agent.is_none());
        assert!(task.model.is_none());
        assert_eq!(task.auto_unblock_count, 1);
        assert!(!task.auto_unblock_last_at.is_empty());
        assert_eq!(task.auto_unblock_last_reason, "RateLimit");
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn auto_unblock_skips_manual_block() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            store.clone(),
            "owner/repo".to_string(),
        ));
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: "88",
                title: "Manually blocked",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::Blocked)
            .await
            .unwrap();
        store
            .set_block_reason(id, Some("waiting on input"))
            .await
            .unwrap();

        let run_id = store
            .start_run(&crate::store::StartRun {
                task_id: id,
                attempt: 1,
                run_type: "agent",
                agent: "codex",
                model: "gpt-5",
                command: "codex --model gpt-5",
                prompt: "do thing",
            })
            .await
            .unwrap();
        store
            .complete_run(&crate::store::CompleteRun {
                run_id,
                exit_code: Some(1),
                stdout: "",
                stderr: "",
                parsed: "",
                outcome: "rate_limit",
                error: "rate limit exceeded",
                tokens: RunTokenUsage::default(),
            })
            .await
            .unwrap();

        auto_unblock_blocked_tasks("owner/repo", &task_manager, &store, &dispatching)
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(task.status, crate::store::TaskStatus::Blocked);
        assert_eq!(task.auto_unblock_count, 0);
    }

    // ── FailureCategory::as_str ─────────────────────────────────────────

    #[serial(cooldown_state)]
    #[test]
    fn failure_category_as_str() {
        // as_str() must return a stable, human-readable string used as the
        // reason key in auto_unblock tracking (stored in auto_unblock_last_reason).
        assert_eq!(FailureCategory::RateLimit.as_str(), "RateLimit");
        assert_eq!(FailureCategory::Timeout.as_str(), "Timeout");
        assert_eq!(
            FailureCategory::ModelUnavailable.as_str(),
            "ModelUnavailable"
        );
        assert_eq!(FailureCategory::MaxAttempts.as_str(), "MaxAttempts");
        assert_eq!(FailureCategory::Unknown.as_str(), "Unknown");
        assert_eq!(FailureCategory::SilentExit0.as_str(), "SilentExit0");
        assert_eq!(FailureCategory::ConnectionError.as_str(), "ConnectionError");
        assert_eq!(FailureCategory::CliFlagError.as_str(), "CliFlagError");
        assert_eq!(
            FailureCategory::AllAgentsExhausted.as_str(),
            "AllAgentsExhausted"
        );
        assert_eq!(FailureCategory::ParseError.as_str(), "ParseError");
        assert_eq!(FailureCategory::FalseFailure.as_str(), "FalseFailure");
        assert_eq!(FailureCategory::PushFailed.as_str(), "PushFailed");
        assert_eq!(FailureCategory::PrCreateFailed.as_str(), "PrCreateFailed");
    }

    // ── classify_failure: ModelUnavailable ──────────────────────────────

    #[serial(cooldown_state)]
    #[test]
    fn classify_failure_model_unavailable_phrase_is_recoverable() {
        let category = classify_failure("model unavailable (anthropic/claude-sonnet-4-6): Model not found: anthropic/claude-sonnet-4-6", "error");
        assert_eq!(
            category,
            FailureCategory::ModelUnavailable,
            "\"model unavailable\" error must classify as ModelUnavailable"
        );
        assert!(
            category.is_recoverable(),
            "ModelUnavailable must be recoverable so auto-unblock can re-route"
        );
    }

    // ── classify_failure: PrCreateFailed ──────────────────────────────
    #[serial(cooldown_state)]
    #[test]
    fn classify_failure_pr_create_failed_specific_patterns() {
        // These should be classified as PrCreateFailed
        assert_eq!(
            classify_failure("create pr failed", ""),
            FailureCategory::PrCreateFailed
        );
        assert_eq!(
            classify_failure("failed to create pull request", ""),
            FailureCategory::PrCreateFailed
        );
        assert_eq!(
            classify_failure("pull request creation failed", ""),
            FailureCategory::PrCreateFailed
        );

        // These should NOT be classified as PrCreateFailed (they were before the fix)
        assert_ne!(
            classify_failure("network failed while fetching pull request status", ""),
            FailureCategory::PrCreateFailed
        );
        assert_ne!(
            classify_failure("connection failed, retrying pull request check", ""),
            FailureCategory::PrCreateFailed
        );
        assert_ne!(
            classify_failure("pull request created but failed to merge", ""),
            FailureCategory::PrCreateFailed
        );
    }

    #[serial(cooldown_state)]
    #[test]
    fn classify_failure_model_not_found_is_recoverable() {
        let category = classify_failure("Model not found: gpt-5-ultra", "error");
        assert_eq!(
            category,
            FailureCategory::ModelUnavailable,
            "\"model not found\" error must classify as ModelUnavailable"
        );
        assert!(category.is_recoverable());
    }

    #[serial(cooldown_state)]
    #[test]
    fn classify_failure_model_does_not_exist_is_recoverable() {
        let category = classify_failure("The model `claude-opus-99` does not exist", "error");
        assert_eq!(category, FailureCategory::ModelUnavailable);
        assert!(category.is_recoverable());
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn auto_unblock_routes_task_blocked_by_model_unavailable() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            store.clone(),
            "owner/repo".to_string(),
        ));
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: "1202",
                title: "Task blocked by model unavailable",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::Blocked)
            .await
            .unwrap();

        let run_id = store
            .start_run(&crate::store::StartRun {
                task_id: id,
                attempt: 1,
                run_type: "agent",
                agent: "opencode",
                model: "anthropic/claude-sonnet-4-6",
                command: "opencode",
                prompt: "fix the bug",
            })
            .await
            .unwrap();
        store
            .complete_run(&crate::store::CompleteRun {
                run_id,
                exit_code: Some(1),
                stdout: "",
                stderr: "",
                parsed: "",
                outcome: "error",
                error: "model unavailable (anthropic/claude-sonnet-4-6): Model not found: anthropic/claude-sonnet-4-6",
                tokens: RunTokenUsage::default(),
            })
            .await
            .unwrap();

        auto_unblock_blocked_tasks("owner/repo", &task_manager, &store, &dispatching)
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(
            task.status,
            TaskStatus::New,
            "task blocked by model unavailable must be auto-unblocked and re-routed"
        );
        assert_eq!(task.auto_unblock_count, 1);
        assert_eq!(task.auto_unblock_last_reason, "ModelUnavailable");
    }

    // ── auto_unblock: reason-reset (regression for #1227) ─────────────────

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn auto_unblock_resets_count_when_reason_changes() {
        crate::engine::cooldown::reset_global_state().await;
        // Regression test for #1227: when a task is blocked for a different failure
        // reason than the last auto-unblock, the counter should reset to 0 (immediate
        // retry) instead of accumulating across unrelated failures.
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            store.clone(),
            "owner/repo".to_string(),
        ));
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: "1227",
                title: "Task with different failure reason",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::Blocked)
            .await
            .unwrap();
        // Simulate: task was previously auto-unblocked for ModelUnavailable (count=1)
        store
            .set_fields(
                id,
                &[
                    ("auto_unblock_count", serde_json::json!(1)),
                    (
                        "auto_unblock_last_reason",
                        serde_json::json!("ModelUnavailable"),
                    ),
                ],
            )
            .await
            .unwrap();

        // Add a run with a DIFFERENT failure: MaxAttempts
        let run_id = store
            .start_run(&crate::store::StartRun {
                task_id: id,
                attempt: 2,
                run_type: "agent",
                agent: "claude",
                model: "opus",
                command: "claude",
                prompt: "fix the bug",
            })
            .await
            .unwrap();
        store
            .complete_run(&crate::store::CompleteRun {
                run_id,
                exit_code: Some(1),
                stdout: "",
                stderr: "",
                parsed: "",
                outcome: "error",
                error: "exceeded max attempts",
                tokens: RunTokenUsage::default(),
            })
            .await
            .unwrap();

        auto_unblock_blocked_tasks("owner/repo", &task_manager, &store, &dispatching)
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(
            task.status,
            TaskStatus::New,
            "task must be auto-unblocked even with count=1 from a different reason"
        );
        // Count is reset to 0, then incremented to 1 for the new reason
        assert_eq!(task.auto_unblock_count, 1);
        assert_eq!(
            task.auto_unblock_last_reason, "MaxAttempts",
            "reason must be updated to the new failure category"
        );
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn auto_unblock_stores_reason_on_first_unblock() {
        crate::engine::cooldown::reset_global_state().await;
        // When a task is auto-unblocked for the first time, the failure reason
        // should be stored so subsequent blocks can be compared.
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            store.clone(),
            "owner/repo".to_string(),
        ));
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: "1227b",
                title: "Task for first-time reason storage",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::Blocked)
            .await
            .unwrap();

        let run_id = store
            .start_run(&crate::store::StartRun {
                task_id: id,
                attempt: 1,
                run_type: "agent",
                agent: "claude",
                model: "opus",
                command: "claude",
                prompt: "fix the bug",
            })
            .await
            .unwrap();
        store
            .complete_run(&crate::store::CompleteRun {
                run_id,
                exit_code: Some(1),
                stdout: "",
                stderr: "",
                parsed: "",
                outcome: "timeout",
                error: "task timed out",
                tokens: RunTokenUsage::default(),
            })
            .await
            .unwrap();

        auto_unblock_blocked_tasks("owner/repo", &task_manager, &store, &dispatching)
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(task.status, TaskStatus::New);
        assert_eq!(task.auto_unblock_count, 1);
        assert_eq!(
            task.auto_unblock_last_reason, "Timeout",
            "reason must be stored on first auto-unblock"
        );
    }

    // ── classify_failure: sparse checkout (regression for substring false positive) ──

    #[serial(cooldown_state)]
    #[test]
    fn classify_failure_sparse_checkout_not_parse_error() {
        // Regression test for #1204: "sparse" substring contains "parse",
        // so overly broad substring match would misclassify git sparse-checkout errors
        let category = classify_failure("error: cannot reset paths in a sparse checkout", "error");
        assert_ne!(
            category,
            FailureCategory::ParseError,
            "sparse checkout errors must NOT be classified as ParseError"
        );
        assert_eq!(
            category,
            FailureCategory::Unknown,
            "sparse checkout errors should be Unknown (not recoverable auto-unblock)"
        );
    }

    #[serial(cooldown_state)]
    #[test]
    fn classify_failure_sparse_index_not_parse_error() {
        // Another sparse-checkout variant that contains "parse"
        let category = classify_failure("failed due to sparse index configuration", "error");
        assert_ne!(category, FailureCategory::ParseError);
        assert_eq!(category, FailureCategory::Unknown);
    }

    // ── classify_failure: truncated (issue #3473) ───────────────────────

    /// A review output truncated by the model's token budget (opencode
    /// step_finish reason=length) must classify as recoverable, so a task
    /// blocked after repeated truncations can be auto-unblocked instead of
    /// requiring manual intervention.
    #[serial(cooldown_state)]
    #[test]
    fn classify_failure_truncated_is_recoverable() {
        let category = classify_failure(
            "truncated: review output was cut off before completion \
             (output/reasoning token budget exceeded)",
            "truncated",
        );
        assert_eq!(
            category,
            FailureCategory::ParseError,
            "truncated outcome must classify as a recoverable parse-style failure"
        );
        assert!(
            category.is_recoverable(),
            "truncated failures must be recoverable so auto-unblock can retry"
        );
    }

    #[serial(cooldown_state)]
    #[test]
    fn dispatching_key_includes_repo_for_cross_project_isolation() {
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

        // Lock task 42 in repo A
        let key_a = "owner/repo-a/42".to_string();
        dispatching.insert(key_a.clone(), "42".to_string());

        // Same task ID in repo B should NOT be blocked
        let key_b = "owner/repo-b/42".to_string();
        assert!(dispatching.contains_key(&key_a));
        assert!(
            !dispatching.contains_key(&key_b),
            "same task ID in different repo should not be blocked"
        );
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn emit_degraded_agents_writes_metric_when_three_or_more_agents_degraded() {
        crate::engine::cooldown::reset_global_state().await;
        use crate::engine::router::{Router, RouterConfig};

        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        // Create a router and set deterministic available agents
        let mut config = RouterConfig::default();
        config
            .model_map
            .entry("medium".to_string())
            .or_default()
            .insert(
                "test-agent-3a".to_string(),
                vec!["test-model-a".to_string()],
            );
        config
            .model_map
            .entry("medium".to_string())
            .or_default()
            .insert(
                "test-agent-3b".to_string(),
                vec!["test-model-b".to_string()],
            );
        config
            .model_map
            .entry("medium".to_string())
            .or_default()
            .insert(
                "test-agent-3c".to_string(),
                vec!["test-model-c".to_string()],
            );
        config
            .model_map
            .entry("medium".to_string())
            .or_default()
            .insert(
                "test-agent-3d".to_string(),
                vec!["test-model-d".to_string()],
            );
        let mut router = Router::new(config);
        router.available_agents = vec![
            "test-agent-3a".to_string(),
            "test-agent-3b".to_string(),
            "test-agent-3c".to_string(),
            "test-agent-3d".to_string(),
        ];

        // Place three agents into cooldown
        set_agent_cooldown("test-agent-3a", 3600).await;
        set_agent_cooldown("test-agent-3b", 3600).await;
        set_agent_cooldown("test-agent-3c", 3600).await;

        // Call the helper and assert KV metrics written
        emit_degraded_agents_if_needed(&router.available_agents, &router.config, Some(&store))
            .await;

        // Count metric always written
        let count_val = store
            .kv_get("metrics:orch.agents_degraded.count")
            .await
            .unwrap();
        assert_eq!(count_val.as_deref(), Some("3"));

        // Alert metric set to "1" when threshold crossed
        let alert_val = store
            .kv_get("metrics:orch.agents_degraded.alert")
            .await
            .unwrap();
        assert_eq!(alert_val.as_deref(), Some("1"));
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn emit_degraded_agents_clears_alert_when_below_threshold() {
        crate::engine::cooldown::reset_global_state().await;
        use crate::engine::router::{Router, RouterConfig};

        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        let mut config = RouterConfig::default();
        config
            .model_map
            .entry("medium".to_string())
            .or_default()
            .insert(
                "test-agent-2a".to_string(),
                vec!["test-model-a".to_string()],
            );
        config
            .model_map
            .entry("medium".to_string())
            .or_default()
            .insert(
                "test-agent-2b".to_string(),
                vec!["test-model-b".to_string()],
            );
        let mut router = Router::new(config);
        router.available_agents = vec!["test-agent-2a".to_string(), "test-agent-2b".to_string()];

        // Place only two agents into cooldown — below the threshold of 3
        set_agent_cooldown("test-agent-2a", 3600).await;
        set_agent_cooldown("test-agent-2b", 3600).await;

        emit_degraded_agents_if_needed(&router.available_agents, &router.config, Some(&store))
            .await;

        // Count metric still written (always-emit)
        let count_val = store
            .kv_get("metrics:orch.agents_degraded.count")
            .await
            .unwrap();
        assert_eq!(count_val.as_deref(), Some("2"));

        // Alert metric cleared (set to "0") — no WARN should fire
        let alert_val = store
            .kv_get("metrics:orch.agents_degraded.alert")
            .await
            .unwrap();
        assert_eq!(alert_val.as_deref(), Some("0"));

        let stale_pool_alert = store
            .kv_get("metrics:orch.model_pool_stale_persistent.alert")
            .await
            .unwrap();
        assert_eq!(stale_pool_alert.as_deref(), Some("0"));
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn emit_degraded_agents_sets_stale_pool_alert_for_persistent_model_failures() {
        crate::engine::cooldown::reset_global_state().await;
        use crate::engine::cooldown::record_persistent_model_failure;
        use crate::engine::router::{Router, RouterConfig};

        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        let mut config = RouterConfig::default();
        config
            .model_map
            .entry("medium".to_string())
            .or_default()
            .insert(
                "test-agent-stale".to_string(),
                vec!["model-a".to_string(), "model-b".to_string()],
            );
        let mut router = Router::new(config);
        router.available_agents = vec!["test-agent-stale".to_string()];

        // 50% cooled + persistent reason should trigger stale-pool alert.
        record_persistent_model_failure("test-agent-stale", "model-a").await;

        emit_degraded_agents_if_needed(&router.available_agents, &router.config, Some(&store))
            .await;

        let stale_pool_alert = store
            .kv_get("metrics:orch.model_pool_stale_persistent.alert")
            .await
            .unwrap();
        assert_eq!(stale_pool_alert.as_deref(), Some("1"));
    }

    // ── classify_comment tests ──────────────────────────────────────────

    use super::{classify_comment, CommentAction, MentionCursor};
    use crate::engine::commands::OwnerCommand;

    #[serial(cooldown_state)]
    #[test]
    fn classify_skip_already_processed() {
        let action = classify_comment(
            "@orch /retry",
            Some("https://api.github.com/repos/o/r/issues/1"),
            "@bot",
            true,
        );
        assert_eq!(action, CommentAction::Skip);
    }

    #[serial(cooldown_state)]
    #[test]
    fn classify_skip_irrelevant_comment() {
        let action = classify_comment("just a regular comment", None, "@bot", false);
        assert_eq!(action, CommentAction::Skip);
    }

    #[serial(cooldown_state)]
    #[test]
    fn classify_command_without_mention() {
        let action = classify_comment(
            "/retry",
            Some("https://api.github.com/repos/o/r/issues/42"),
            "@bot",
            false,
        );
        assert_eq!(
            action,
            CommentAction::ExecuteCommand {
                command: OwnerCommand::Retry,
                issue_num: "42".to_string(),
            }
        );
    }

    #[serial(cooldown_state)]
    #[test]
    fn classify_command_with_mention() {
        let action = classify_comment(
            "@orch /close",
            Some("https://api.github.com/repos/o/r/issues/7"),
            "@bot",
            false,
        );
        assert_eq!(
            action,
            CommentAction::ExecuteCommandForMention {
                command: OwnerCommand::Close,
                issue_num: "7".to_string(),
            }
        );
    }

    #[serial(cooldown_state)]
    #[test]
    fn classify_mention_without_command() {
        let action = classify_comment(
            "@orch can you look at this?",
            Some("https://api.github.com/repos/o/r/issues/99"),
            "@bot",
            false,
        );
        assert_eq!(
            action,
            CommentAction::CreateMentionTask {
                issue_num: Some("99".to_string()),
            }
        );
    }

    #[serial(cooldown_state)]
    #[test]
    fn classify_mention_without_issue_url() {
        let action = classify_comment("@orch help me", None, "@bot", false);
        assert_eq!(action, CommentAction::CreateMentionTask { issue_num: None });
    }

    #[serial(cooldown_state)]
    #[test]
    fn classify_command_no_mention_no_url_skips() {
        let action = classify_comment("/retry", None, "@bot", false);
        assert_eq!(action, CommentAction::Skip);
    }

    #[serial(cooldown_state)]
    #[test]
    fn classify_mention_via_current_user() {
        let action = classify_comment(
            "@bot please help",
            Some("https://api.github.com/repos/o/r/issues/5"),
            "@bot",
            false,
        );
        assert_eq!(
            action,
            CommentAction::CreateMentionTask {
                issue_num: Some("5".to_string()),
            }
        );
    }

    #[serial(cooldown_state)]
    #[test]
    fn classify_command_with_mention_no_url_creates_task() {
        // @mention + command but no issue URL → can't run command, create mention task
        let action = classify_comment("@orch /retry", None, "@bot", false);
        assert_eq!(action, CommentAction::CreateMentionTask { issue_num: None });
    }

    #[serial(cooldown_state)]
    #[test]
    fn mention_cursor_stops_advancing_after_gap() {
        let mut cursor = MentionCursor::default();
        cursor.advance("2026-01-01T00:00:00Z");
        cursor.block_on_gap();
        cursor.advance("2026-01-02T00:00:00Z");

        assert_eq!(
            cursor.into_last_safe_ts().as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
    }

    #[serial(cooldown_state)]
    #[test]
    fn mention_cursor_tracks_max_timestamp_until_gap() {
        let mut cursor = MentionCursor::default();
        cursor.advance("2026-01-01T00:00:00Z");
        cursor.advance("2026-01-01T00:05:00Z");
        cursor.block_on_gap();
        cursor.advance("2026-01-01T00:10:00Z");

        assert_eq!(
            cursor.into_last_safe_ts().as_deref(),
            Some("2026-01-01T00:05:00Z")
        );
    }

    // ── scan_comments: acknowledge_mention failure does not advance cursor ────

    /// Backend that tracks acknowledge_mention calls and can be configured to fail them.
    struct AckTrackingBackend {
        ack_calls: tokio::sync::Mutex<Vec<String>>,
        ack_failure: bool,
        inner: crate::backends::test_helpers::NoopBackend,
    }

    impl AckTrackingBackend {
        fn new(ack_failure: bool) -> Arc<Self> {
            Arc::new(Self {
                ack_calls: tokio::sync::Mutex::new(Vec::new()),
                ack_failure,
                inner: crate::backends::test_helpers::NoopBackend,
            })
        }

        fn fail_on_ack(&self) -> bool {
            self.ack_failure
        }
    }

    #[async_trait]
    impl ExternalBackend for AckTrackingBackend {
        fn name(&self) -> &str {
            "ack-tracking"
        }

        async fn acknowledge_mention(&self, comment_id: &str) -> anyhow::Result<()> {
            self.ack_calls.lock().await.push(comment_id.to_string());
            if self.fail_on_ack() {
                // Return a transient error to simulate network glitch.
                anyhow::bail!("network error: temporary failure posting reaction");
            }
            Ok(())
        }

        // Override get_authenticated_user to ensure scan proceeds.
        async fn get_authenticated_user(&self) -> anyhow::Result<Option<String>> {
            Ok(Some("bot".into()))
        }

        // Override get_mentions to return empty (we use prefetched comments).
        async fn get_mentions(&self, _since: &str) -> anyhow::Result<Vec<Mention>> {
            Ok(vec![])
        }

        // Delegate everything else to NoopBackend.
        async fn create_task(&self, t: &str, b: &str, l: &[String]) -> anyhow::Result<ExternalId> {
            self.inner.create_task(t, b, l).await
        }
        async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask> {
            self.inner.get_task(id).await
        }
        async fn list_by_status(&self, s: Status) -> anyhow::Result<Vec<ExternalTask>> {
            self.inner.list_by_status(s).await
        }
        async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
            self.inner.list_routable().await
        }
        async fn post_comment(&self, id: &ExternalId, b: &str) -> anyhow::Result<()> {
            self.inner.post_comment(id, b).await
        }
        async fn set_labels(&self, id: &ExternalId, l: &[String]) -> anyhow::Result<()> {
            self.inner.set_labels(id, l).await
        }
        async fn remove_label(&self, id: &ExternalId, l: &str) -> anyhow::Result<()> {
            self.inner.remove_label(id, l).await
        }
        async fn get_sub_issues(&self, id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
            self.inner.get_sub_issues(id).await
        }
        async fn create_sub_task(
            &self,
            p: &ExternalId,
            t: &str,
            b: &str,
            l: &[String],
        ) -> anyhow::Result<ExternalId> {
            self.inner.create_sub_task(p, t, b, l).await
        }
        async fn ensure_status_label(&self, l: &str) -> anyhow::Result<()> {
            self.inner.ensure_status_label(l).await
        }
        async fn has_open_issue_with_title(&self, t: &str, l: &str) -> anyhow::Result<bool> {
            self.inner.has_open_issue_with_title(t, l).await
        }
        async fn health_check(&self) -> anyhow::Result<()> {
            self.inner.health_check().await
        }
        async fn is_pr_merged(&self, b: &str) -> anyhow::Result<bool> {
            self.inner.is_pr_merged(b).await
        }
        async fn update_status(&self, id: &ExternalId, s: Status) -> anyhow::Result<()> {
            self.inner.update_status(id, s).await
        }
    }

    /// When acknowledge_mention fails for CreateMentionTask, the cursor must NOT
    /// advance and no mention task should be created. The next tick will retry.
    #[serial(cooldown_state)]
    #[tokio::test]
    async fn create_mention_task_skips_on_ack_failure() {
        crate::engine::cooldown::reset_global_state().await;
        let backend: Arc<AckTrackingBackend> = AckTrackingBackend::new(true);
        let store: Arc<TaskStore> = Arc::new(TaskStore::open_memory().await.unwrap());

        // Set an initial cursor so we can verify it wasn't advanced.
        let initial_ts = "2026-01-01T00:00:00Z";
        store
            .kv_set("mentions_last_checked", initial_ts)
            .await
            .unwrap();

        let mention = Mention {
            id: "mention-abc".into(),
            body: "@bot can you help?".into(),
            author: "alice".into(),
            created_at: "2026-01-02T00:00:00Z".into(),
            issue_url: Some("https://api.github.com/repos/owner/repo/issues/42".into()),
        };

        let backend_trait: Arc<dyn ExternalBackend> = backend.clone();
        let task_manager = Arc::new(TaskManager::new(backend_trait.clone()));

        scan_comments(
            &backend_trait,
            Some(&store),
            "owner/repo",
            &task_manager,
            Some(&[mention]),
        )
        .await
        .unwrap();

        // acknowledge_mention was called with the mention ID.
        let calls = backend.ack_calls.lock().await;
        assert_eq!(
            &**calls,
            &["mention-abc"],
            "acknowledge_mention must be called"
        );

        // No mention task was created.
        let all = store.list_all("owner/repo").await.unwrap();
        assert!(
            all.is_empty(),
            "no task should be created when acknowledge_mention fails"
        );

        // Cursor was NOT advanced.
        let cursor = store.kv_get("mentions_last_checked").await.unwrap();
        assert_eq!(
            cursor.as_deref(),
            Some(initial_ts),
            "cursor must not advance when acknowledge_mention fails"
        );
    }

    /// When acknowledge_mention fails for ExecuteCommandForMention, handle_slash_command
    /// must NOT be called and no mention task should be created.
    #[serial(cooldown_state)]
    #[tokio::test]
    async fn execute_command_for_mention_skips_on_ack_failure() {
        crate::engine::cooldown::reset_global_state().await;
        let backend: Arc<AckTrackingBackend> = AckTrackingBackend::new(true);
        let store: Arc<TaskStore> = Arc::new(TaskStore::open_memory().await.unwrap());

        let initial_ts = "2026-01-01T00:00:00Z";
        store
            .kv_set("mentions_last_checked", initial_ts)
            .await
            .unwrap();

        // Mention with a slash command and issue URL → ExecuteCommandForMention.
        let mention = Mention {
            id: "mention-def".into(),
            body: "@bot /close".into(),
            author: "alice".into(),
            created_at: "2026-01-02T00:00:00Z".into(),
            issue_url: Some("https://api.github.com/repos/owner/repo/issues/42".into()),
        };

        let backend_trait: Arc<dyn ExternalBackend> = backend.clone();
        let task_manager = Arc::new(TaskManager::new(backend_trait.clone()));

        scan_comments(
            &backend_trait,
            Some(&store),
            "owner/repo",
            &task_manager,
            Some(&[mention]),
        )
        .await
        .unwrap();

        // acknowledge_mention was called.
        let calls = backend.ack_calls.lock().await;
        assert_eq!(&**calls, &["mention-def"]);

        // No mention task was created (handle_slash_command was not called).
        let all = store.list_all("owner/repo").await.unwrap();
        assert!(
            all.is_empty(),
            "no task should be created when acknowledge_mention fails for ExecuteCommandForMention"
        );

        // Cursor was NOT advanced.
        let cursor = store.kv_get("mentions_last_checked").await.unwrap();
        assert_eq!(
            cursor.as_deref(),
            Some(initial_ts),
            "cursor must not advance when acknowledge_mention fails for ExecuteCommandForMention"
        );
    }

    // ── is_ci_failure_block ────────────────────────────────────────────────

    fn make_task(block_reason: Option<String>) -> crate::store::Task {
        crate::store::Task {
            id: 1,
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: String::new(),
            title: String::new(),
            body: String::new(),
            status: crate::store::TaskStatus::Blocked,
            source: String::new(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            agent: None,
            model: None,
            complexity: String::new(),
            estimate: 0,
            route_reason: String::new(),
            agent_profile: String::new(),
            selected_skills: String::new(),
            route_attempts: 0,
            attempts: 0,
            branch: String::new(),
            worktree: String::new(),
            worktree_cleaned: false,
            summary: String::new(),
            last_error: String::new(),
            parent_id: None,
            block_reason,
            pr_number: Some(42),
            pr_review_context: String::new(),
            last_review_ts: String::new(),
            review_ts_map: String::new(),
            last_comment_review_ts: String::new(),
            merge_conflict_retries: 0,
            ci_merge_failures: 0,
            pr_create_failures: 0,
            push_failures: 0,
            network_retries: 0,
            review_agent_failures: 0,
            review_cycles: 0,
            review_invocations: 0,
            review_session_expected: false,
            needs_review_refires: 0,
            input_tokens: 0,
            output_tokens: 0,
            input_cost_usd: 0.0,
            output_cost_usd: 0.0,
            total_cost_usd: 0.0,
            model_reroute_chain: String::new(),
            limit_reroute_chain: String::new(),
            memory: vec![],
            delegations: vec![],
            auto_unblock_count: 0,
            auto_unblock_last_at: String::new(),
            auto_unblock_last_reason: String::new(),
            ci_recovery_count: 0,
            no_code_reroutes: 0,
            no_code_last_agent: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[serial(cooldown_state)]
    #[test]
    fn is_ci_failure_block_matches_ci_failure_limit() {
        let task = make_task(Some(
            "CI failure limit (3) reached during auto-merge".to_string(),
        ));
        assert!(is_ci_failure_block(&task));
    }

    #[serial(cooldown_state)]
    #[test]
    fn is_ci_failure_block_matches_ci_checks_timeout() {
        let task = make_task(Some(
            "CI checks timed out after 3 auto-merge failures".to_string(),
        ));
        assert!(is_ci_failure_block(&task));
    }

    #[serial(cooldown_state)]
    #[test]
    fn is_ci_failure_block_false_for_regular_block() {
        let task = make_task(Some("waiting on input".to_string()));
        assert!(!is_ci_failure_block(&task));
    }

    #[serial(cooldown_state)]
    #[test]
    fn is_ci_failure_block_false_for_none_reason() {
        let task = make_task(None);
        assert!(!is_ci_failure_block(&task));
    }

    // ── ci_failure_unblock_cooldown_elapsed ────────────────────────────────

    #[serial(cooldown_state)]
    #[test]
    fn ci_failure_cooldown_zero_count_immediate() {
        // count=0 means never attempted — always allow immediate attempt
        let task = make_task(None);
        assert!(ci_failure_unblock_cooldown_elapsed(&task));
    }

    #[serial(cooldown_state)]
    #[test]
    fn ci_failure_cooldown_empty_last_at_immediate() {
        // count=1 but empty last_at means it was never recorded — allow immediate
        let mut task = make_task(None);
        task.auto_unblock_count = 1;
        assert!(ci_failure_unblock_cooldown_elapsed(&task));
    }

    #[serial(cooldown_state)]
    #[test]
    fn ci_failure_cooldown_invalid_timestamp_immediate() {
        // invalid timestamp should not block
        let mut task = make_task(None);
        task.auto_unblock_count = 1;
        task.auto_unblock_last_at = "not-a-timestamp".to_string();
        assert!(ci_failure_unblock_cooldown_elapsed(&task));
    }

    #[serial(cooldown_state)]
    #[test]
    fn ci_failure_cooldown_high_count_still_eventually_elapses() {
        // count >= 3 must NOT be a permanent block — verify a sufficiently old
        // timestamp eventually elapses the cooldown.
        let long_ago = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
        for count in [3i32, 5, 99] {
            let mut task = make_task(None);
            task.auto_unblock_count = count;
            task.auto_unblock_last_at = long_ago.clone();
            assert!(
                ci_failure_unblock_cooldown_elapsed(&task),
                "count={count} should eventually elapse"
            );
        }
    }

    #[serial(cooldown_state)]
    #[test]
    fn ci_failure_cooldown_recent_timestamp_still_blocks() {
        // A recent timestamp should still gate the check at every count.
        let just_now = chrono::Utc::now().to_rfc3339();
        for count in [1i32, 2, 3, 99] {
            let mut task = make_task(None);
            task.auto_unblock_count = count;
            task.auto_unblock_last_at = just_now.clone();
            assert!(
                !ci_failure_unblock_cooldown_elapsed(&task),
                "count={count} should still be cooling down"
            );
        }
    }

    // ── auto_unblock_ci_failure_blocked_tasks_global ───────────────────────

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn global_ci_failure_sweep_returns_ok_with_no_blocked_tasks() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            store.clone(),
            "owner/repo".to_string(),
        ));
        auto_unblock_ci_failure_blocked_tasks_global(&task_manager, &store)
            .await
            .unwrap();
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn global_ci_failure_sweep_skips_tasks_in_cooldown() {
        // Tasks with a fresh auto_unblock_last_at are skipped before reaching GhHttp,
        // so this test works without a real GitHub token.
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            store.clone(),
            "active/repo".to_string(),
        ));

        // Insert a CI-failure task from an inactive repo with a very recent last_at.
        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "inactive/repo",
                ext_id: "10",
                title: "Old blocked task",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::Blocked)
            .await
            .unwrap();
        let just_now = chrono::Utc::now().to_rfc3339();
        store
            .set_fields(
                id,
                &[
                    (
                        "block_reason",
                        serde_json::json!("CI failure limit reached during auto-merge"),
                    ),
                    ("pr_number", serde_json::json!(99)),
                    ("auto_unblock_count", serde_json::json!(1)),
                    ("auto_unblock_last_at", serde_json::json!(just_now)),
                ],
            )
            .await
            .unwrap();

        auto_unblock_ci_failure_blocked_tasks_global(&task_manager, &store)
            .await
            .unwrap();

        // Cooldown not elapsed — task must remain untouched.
        let task = store.get(id).await.unwrap();
        assert_eq!(
            task.auto_unblock_count, 1,
            "task in cooldown must not be processed"
        );
    }

    #[serial(cooldown_state)]
    #[tokio::test]
    async fn global_ci_failure_sweep_ignores_non_ci_failure_blocks() {
        crate::engine::cooldown::reset_global_state().await;
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            store.clone(),
            "active/repo".to_string(),
        ));

        // Insert a non-CI-failure blocked task — should be ignored entirely.
        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "inactive/repo",
                ext_id: "20",
                title: "Manual block",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::Blocked)
            .await
            .unwrap();
        store
            .set_fields(
                id,
                &[("block_reason", serde_json::json!("waiting for human input"))],
            )
            .await
            .unwrap();

        auto_unblock_ci_failure_blocked_tasks_global(&task_manager, &store)
            .await
            .unwrap();

        // Non-CI-failure tasks must remain blocked.
        let task = store.get(id).await.unwrap();
        assert_eq!(task.status, crate::store::TaskStatus::Blocked);
    }
}
