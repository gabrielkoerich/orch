//! Sync tick — periodic operations that run less frequently than the core tick.
//!
//! The sync tick runs every ~45 seconds and handles:
//! - Worktree cleanup for done tasks
//! - Merged-PR detection
//! - @mention scanning and internal task creation
//! - PR review processing and follow-up dispatch
//! - Stale InReview detection
//! - Owner /slash command scanning

mod comments;
mod unblock;

use super::EngineConfig;
use crate::backends::ExternalTask;
use crate::backends::{ExternalBackend, Status};
use crate::cooldown;
use crate::cooldown::{
    cooldown_reason, github_circuit_remaining_secs, is_agent_in_cooldown, is_github_circuit_open,
    is_model_in_cooldown, refresh_degraded_agents,
};
use crate::engine;
use crate::engine::agents::patterns;
use crate::engine::ci::bootstrap;
use crate::engine::git::cleanup::ReviewTaskSnapshot;
use crate::engine::git::cleanup::{check_merged_prs, cleanup_done_worktrees, prune_old_run_output};
use crate::engine::review::poll::review_open_prs;
use crate::engine::router::Router;
use crate::engine::router::RouterConfig;
use crate::engine::router::config as router_config;
use crate::engine::tasks;
use crate::engine::tasks::dependencies;
use crate::engine::tasks::{TaskManager, status_to_task_status};
use crate::github::http::GhHttp;
use crate::store;
use crate::store::Task;
use crate::store::TaskRun;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use crate::sys::tmux::TmuxManager;
use comments::{fetch_comments_since, filter_mentions_by_since, scan_comments};
use dashmap::{DashMap, DashSet};
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use unblock::{REBROADCAST_BLOCK_REASON, auto_unblock_blocked_tasks};
pub(crate) use unblock::{
    auto_recover_rebroadcast_blocked_tasks_global, auto_unblock_ci_failure_blocked_tasks_global,
};

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

/// Global sweep: re-fire stale NeedsReview tasks and escalate persistently-stuck ones
/// to Blocked, across all repos.
///
/// Two cases where the event-driven subscriber in `subscribers/review/mod.rs` can miss
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

        if let Err(e) = bootstrap::apply_pending(store, task_manager, repo).await {
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

        match store.upsert_external(&task.upsert_row(repo)).await {
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

#[cfg(test)]
mod unblock_tests;

#[cfg(test)]
mod comments_tests;
