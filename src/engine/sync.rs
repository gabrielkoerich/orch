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

use crate::backends::{ExternalBackend, ExternalId, Status};
use crate::cmd::CommandErrorContext;
use crate::config;
use crate::engine::cooldown::{
    cooldown_reason, github_circuit_remaining_secs, is_agent_in_cooldown, is_github_circuit_open,
    is_model_in_cooldown,
};
use crate::engine::router::Router;
use crate::engine::runner::agents::patterns;
use crate::engine::tasks::{status_to_task_status, TaskManager};
use crate::store;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use crate::tmux::TmuxManager;
use dashmap::DashSet;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::RwLock;

/// Read a KV value from the store.
async fn kv_get_prefer_store(store: &Option<&Arc<TaskStore>>, key: &str) -> Option<String> {
    if let Some(s) = store {
        if let Ok(v) = s.kv_get(key).await {
            return v;
        }
    }
    None
}

/// Write a KV value to the store.
async fn kv_set_prefer_store(store: &Option<&Arc<TaskStore>>, key: &str, value: &str) {
    if let Some(s) = store {
        if let Err(e) = s.kv_set(key, value).await {
            tracing::warn!(key, err = %e, "kv_set failed");
        }
    }
}

use super::cleanup::{check_merged_prs, cleanup_done_worktrees};
use super::commands::{execute_command, parse_command};
use super::review_poll::review_open_prs;
use super::EngineConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureCategory {
    FalseFailure,
    RateLimit,
    SilentExit0,
    Timeout,
    ConnectionError,
    CliFlagError,
    AllAgentsExhausted,
    ParseError,
    MaxAttempts,
    TokenBudgetExceeded,
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
            FailureCategory::SilentExit0 => "SilentExit0",
            FailureCategory::Timeout => "Timeout",
            FailureCategory::ConnectionError => "ConnectionError",
            FailureCategory::CliFlagError => "CliFlagError",
            FailureCategory::AllAgentsExhausted => "AllAgentsExhausted",
            FailureCategory::ParseError => "ParseError",
            FailureCategory::MaxAttempts => "MaxAttempts",
            FailureCategory::TokenBudgetExceeded => "TokenBudgetExceeded",
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

    if error.trim().is_empty() {
        return FailureCategory::FalseFailure;
    }

    if lower.contains("token budget exceeded") {
        return FailureCategory::TokenBudgetExceeded;
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
        || lower.contains("does not exist") && lower.contains("model")
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
        || lower.contains("invalid response")
        || lower.contains("parse error")
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

async fn auto_unblock_blocked_tasks(
    repo: &str,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    dispatching: &Arc<DashSet<String>>,
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

    for task in blocked {
        if task.block_reason.is_some() {
            continue;
        }

        if task.budget_exceeded
            || task
                .last_error
                .to_lowercase()
                .contains("token budget exceeded")
        {
            continue;
        }

        let id_fallback = task.id.to_string();
        let dispatch_key = format!(
            "{}/{}",
            repo,
            task.external_id.as_deref().unwrap_or(&id_fallback)
        );
        if dispatching.contains(&dispatch_key) {
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

        // Write all field updates in a single atomic set_fields call, ensuring counter,
        // timestamp, and reason are updated together. No intermediate states observable
        // by concurrent ticks.
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
        if has_review_failure {
            fields.push(("review_agent_failures", serde_json::json!(0)));
            fields.push(("review_invocations", serde_json::json!(0)));
        }
        if let Err(e) = store.set_fields(task.id, &fields).await {
            tracing::warn!(task_id = task.id, err = %e, "failed to set auto_unblock fields (including counter) — skipping unblock, counter not incremented");
            continue;
        }

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

        if let Err(e) = task_manager
            .update_task_status(&crate::backends::ExternalId(ext_id), new_status)
            .await
        {
            tracing::warn!(
                task_id = task.id,
                err = %e,
                "auto-unblock failed to update status"
            );
        } else {
            tracing::info!(
                task_id = task.id,
                failures = ?failures,
                review_failure = has_review_failure,
                "auto-unblocked task with recoverable failures"
            );
        }
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
    dispatching: &Arc<DashSet<String>>,
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
        kv_set_prefer_store(&Some(store), "metrics:orch.github_5xx_circuit.open", "1").await;
    } else {
        kv_set_prefer_store(&Some(store), "metrics:orch.github_5xx_circuit.open", "0").await;
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

    // 2. Check for merged PRs (in_review → done)
    if !gh_circuit_open {
        if let Err(e) = check_merged_prs(backend, repo, store, task_manager).await {
            tracing::warn!(err = %e, "PR merge check failed");
        }
    }

    // 3. Scan for @mentions
    if !gh_circuit_open {
        if let Err(e) = scan_mentions(backend, Some(store), repo, task_manager).await {
            tracing::warn!(err = %e, "mention scan failed");
        }
    }

    // 4. Review open PRs (parse review comments, create follow-ups)
    if !gh_circuit_open {
        if let Err(e) = review_open_prs(
            backend,
            repo,
            config,
            task_manager,
            store,
            dispatching,
            auto_merge_in_flight,
        )
        .await
        {
            tracing::warn!(err = %e, "PR review failed");
        }
    }

    // 5. Detect stale InReview tasks and recover them.
    //
    // NeedsReview → InReview triggering is handled exclusively by the event-driven
    // subscriber (`src/engine/subscribers/review.rs`). Having the sync tick do the
    // same thing caused a race: both paths could fire simultaneously, each spawning
    // a review agent, each incrementing the failure counter on a single real failure —
    // prematurely blocking tasks at half the expected retry budget (issue #857).
    let enable_review = config::get("workflow.enable_review_agent")
        .map(|v| v != "false")
        .unwrap_or(true);
    if enable_review {
        // Detect stale InReview tasks (review agent crashed, no active tmux session).
        // Read from the store (includes both external and internal tasks).
        let in_review_tasks = task_manager
            .list_all_by_status(Status::InReview)
            .await
            .unwrap_or_default();
        // Fetch all live tmux sessions once instead of one subprocess call per task.
        let live_sessions: std::collections::HashSet<String> = tmux
            .list_sessions()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.name)
            .collect();
        for task in in_review_tasks {
            // Skip tasks currently being processed by the main tick (dispatch + review flow).
            let dispatch_key = format!("{}/{}", repo, task.id.0);
            {
                if dispatching.contains(&dispatch_key) {
                    tracing::debug!(
                        task_id = task.id.0,
                        "task locked by dispatch flow, skipping stale check"
                    );
                    continue;
                }
            }
            // Skip tasks that just transitioned to InReview — allow time for the
            // review agent to start its tmux session before treating it as stale.
            // A task is only considered stale if it has been in InReview for > 1 minute.
            const MIN_STALE_MINUTES: i64 = 1;
            match chrono::DateTime::parse_from_rfc3339(&task.updated_at) {
                Ok(updated_at) => {
                    let age = chrono::Utc::now() - updated_at.with_timezone(&chrono::Utc);
                    if age.num_minutes() < MIN_STALE_MINUTES {
                        tracing::debug!(
                            task_id = task.id.0,
                            age_seconds = age.num_seconds(),
                            "InReview task is too young to be considered stale, skipping"
                        );
                        continue;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = task.id.0,
                        ts = %task.updated_at,
                        err = %e,
                        "invalid updated_at timestamp — treating task as potentially stale"
                    );
                }
            }

            let review_task_id = format!("{}-review", task.id.0);
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
                    task_id = task.id.0,
                    session = %review_session,
                    "InReview task has no active review session — resetting to NeedsReview"
                );
                // Reset the failure counter: stale-session recovery is an infrastructure
                // event (tmux crash, service restart) not a genuine agent parse failure.
                // Keeping the counter would unfairly consume the budget for the next cycle.
                store::store_set(
                    &Some(Arc::clone(store)),
                    repo,
                    &task.id.0,
                    &[("review_agent_failures", serde_json::json!(0))],
                )
                .await;
                match task_manager
                    .update_task_status_if(&task.id, Status::NeedsReview, Status::InReview)
                    .await
                {
                    Err(e) => {
                        tracing::error!(task_id = %task.id.0, err = %e, "failed to reset stale InReview task — task may be stuck in InReview indefinitely");
                    }
                    Ok(false) => {
                        tracing::debug!(
                            task_id = %task.id.0,
                            "stale InReview reset skipped — task already transitioned (concurrent Done/Blocked/InProgress)"
                        );
                    }
                    Ok(true) => {
                        store::set_review_session_expected(store, repo, &task.id.0, false).await;
                    }
                }
            }
        }
    }

    // 5b. Re-fire events for stale NeedsReview tasks.
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
        let needs_review_tasks = task_manager
            .list_all_by_status(Status::NeedsReview)
            .await
            .unwrap_or_default();

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
        for task in needs_review_tasks {
            // Only retry tasks that have been in NeedsReview long enough that the
            // subscriber should have handled them by now. Fresh tasks (just transitioned)
            // are likely still in flight via the event bus. Unparseable timestamps are
            // treated as stale (fall through to retry).
            let age_minutes =
                if let Ok(updated_at) = chrono::DateTime::parse_from_rfc3339(&task.updated_at) {
                    let age = chrono::Utc::now() - updated_at.with_timezone(&chrono::Utc);
                    if age.num_minutes() < MIN_STALE_NEEDS_REVIEW_MINUTES {
                        continue;
                    }
                    Some(age.num_minutes())
                } else {
                    None
                };

            // Skip tasks actively being dispatched (subscriber is working on them).
            let dispatch_key = format!("{}/{}", repo, task.id.0);
            if dispatching.contains(&dispatch_key) {
                continue;
            }
            // Decide whether to re-fire now using exponential backoff based on a per-task counter.
            // Fetch the store task to read the current needs_review_refires counter without
            // touching updated_at before we know whether we will actually fire.
            let store_task =
                crate::store::opt_store_get_task(&Some(Arc::clone(store)), repo, &task.id.0).await;
            let current_refires = store_task
                .as_ref()
                .map(|t| t.needs_review_refires as u64)
                .unwrap_or(0);
            let new_refires = current_refires + 1;

            // If we've exceeded max attempts, escalate the task to Blocked with a clear reason.
            if new_refires >= MAX_NEEDS_REVIEW_REFIRE_ATTEMPTS {
                // Increment first so the escalation value is accurate.
                let _ = crate::store::store_increment(
                    &Some(Arc::clone(store)),
                    repo,
                    &task.id.0,
                    "needs_review_refires",
                )
                .await;
                tracing::warn!(
                    task_id = task.id.0,
                    new_refires,
                    "escalating NeedsReview task to Blocked after repeated refires"
                );
                // Write block reason and last_error (best-effort)
                crate::store::store_set(
                    &Some(Arc::clone(store)),
                    repo,
                    &task.id.0,
                    &[
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
                    ],
                )
                .await;
                if let Err(e) = task_manager
                    .update_task_status(&task.id, Status::Blocked)
                    .await
                {
                    tracing::warn!(task_id = task.id.0, err = %e, "failed to set Blocked during escalation");
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
                // Not firing yet — bump the counter without touching updated_at so the
                // age calculation remains valid on the next tick.
                let _ = crate::store::store_increment_no_ts(
                    &Some(Arc::clone(store)),
                    repo,
                    &task.id.0,
                    "needs_review_refires",
                )
                .await;
                tracing::debug!(
                    task_id = task.id.0,
                    age_minutes,
                    current_refires,
                    required_minutes,
                    "sync catch-up: delaying NeedsReview re-fire due to backoff"
                );
                continue;
            }

            // Fire: increment counter (this also updates updated_at, giving the task a fresh
            // timestamp so subsequent ticks don't immediately re-fire it again).
            let fired_refires = match crate::store::store_increment(
                &Some(Arc::clone(store)),
                repo,
                &task.id.0,
                "needs_review_refires",
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        task_id = task.id.0,
                        err = %e,
                        "failed to increment needs_review_refires — proceeding with re-fire anyway"
                    );
                    new_refires
                }
            };

            tracing::info!(
                task_id = task.id.0,
                age_minutes,
                refires = fired_refires,
                required_minutes,
                "sync catch-up: re-firing NeedsReview event for stale task"
            );

            if let Err(e) = task_manager
                .update_task_status(&task.id, Status::NeedsReview)
                .await
            {
                tracing::warn!(
                    task_id = task.id.0,
                    err = %e,
                    "sync catch-up: failed to re-fire NeedsReview event"
                );
            }
        }
    }

    // 5c. Auto-unblock tasks with recoverable failures.
    if let Err(e) = auto_unblock_blocked_tasks(repo, task_manager, store, dispatching).await {
        tracing::warn!(err = %e, "auto-unblock failed");
    }

    // 6. Scan for owner /slash commands in issue comments
    if !gh_circuit_open {
        if let Err(e) =
            super::commands::scan_commands(backend, repo, &Some(Arc::clone(store)), task_manager)
                .await
        {
            tracing::warn!(err = %e, "owner command scan failed");
        }
    }

    // 7. Sync skill repositories
    match skills_sync().await {
        Ok(()) => {
            // Invalidate the LLM router's skills catalog cache so new/updated
            // skill files are picked up on the next routing call.
            router.read().await.invalidate_skills_catalog().await;
        }
        Err(e) => {
            tracing::warn!(err = %e, "skills sync failed");
        }
    }

    // Pre-emptive health check: refresh degraded-agent flags from rate_limits table.
    {
        let r = router.read().await;
        r.refresh_health(store).await;
    }

    // Emit degraded-agents metric/log if needed (best-effort)
    let r = router.read().await;
    emit_degraded_agents_if_needed(&r, Some(store)).await;

    Ok(())
}

/// Scan for @mentions and handle them.
///
/// Checks recent issue comments for @orch (and legacy @orchestrator) mentions.
/// If a mention contains a slash command and the author is authorized, executes it.
/// Otherwise, creates an internal task for the mention.
async fn scan_mentions(
    backend: &Arc<dyn ExternalBackend>,
    store: Option<&Arc<TaskStore>>,
    repo: &str,
    task_manager: &Arc<TaskManager>,
) -> anyhow::Result<()> {
    // Get the current user (for mention detection)
    let current_user = match backend.get_authenticated_user().await {
        Ok(Some(u)) => format!("@{}", u),
        Ok(None) => {
            tracing::debug!("backend does not support user identity, skipping mentions");
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(err = %e, "failed to get current user, skipping mentions");
            return Ok(());
        }
    };

    // Use persisted cursor if available, otherwise fall back to 24h ago
    let fallback = chrono::Utc::now() - chrono::Duration::hours(24);
    let since_str = match kv_get_prefer_store(&store, "mentions_last_checked").await {
        Some(ts) => ts,
        None => fallback.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    };

    let mentions = match backend.get_mentions(&since_str).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(err = %e, "failed to get mentions");
            return Ok(());
        }
    };

    // Get existing mention tasks across ALL statuses to avoid duplicates.
    // Uses a targeted source_id-only query to avoid fetching all 57 task columns.
    let existing_mentions: std::collections::HashSet<String> = if let Some(s) = store {
        s.list_source_ids_by_source(repo, "mention")
            .await
            .unwrap_or_default()
            .into_iter()
            .collect()
    } else {
        std::collections::HashSet::new()
    };

    let gh = crate::github::http::GhHttp::new()?;

    // Track the latest timestamp of a successfully processed mention so the
    // cursor only advances past mentions that were actually handled.  If a
    // `create_internal` call fails we leave the cursor before that mention so
    // the next tick retries it instead of permanently losing it.
    let mut last_success_ts: Option<String> = None;

    for mention in mentions {
        // Skip if already processed
        if existing_mentions.contains(&mention.id) {
            continue;
        }

        if !mention.body.contains(&current_user)
            && !mention.body.contains("@orch")
            && !mention.body.contains("@orchestrator")
        {
            continue;
        }

        // Acknowledge the mention with an eyes reaction (non-fatal)
        if let Err(e) = backend.acknowledge_mention(&mention.id).await {
            tracing::debug!(err = %e, mention_id = %mention.id, "failed to acknowledge mention");
        }

        // Create internal task for this mention
        // Try to parse a slash command from the mention
        if let Some(command) = parse_command(&mention.body) {
            // Extract issue number from the mention's issue URL
            let issue_number: Option<String> = match mention.issue_url.as_deref() {
                Some(url) => match url.rsplit('/').next() {
                    Some(num) if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) => {
                        Some(num.to_string())
                    }
                    _ => {
                        tracing::warn!(
                            comment_id = %mention.id,
                            "slash command in mention without valid issue number, creating task instead"
                        );
                        None
                    }
                },
                None => {
                    tracing::warn!(
                        comment_id = %mention.id,
                        "slash command in mention without issue_url, creating task instead"
                    );
                    None
                }
            };

            // If we have an issue number, try to execute the command
            if let Some(issue_num) = issue_number {
                // Check if issue is still open
                match gh.get_issue(repo, &issue_num).await {
                    Ok(issue) if issue.state != "open" => {
                        tracing::debug!(
                            issue = %issue_num,
                            state = %issue.state,
                            command = %command,
                            "ignoring slash command in mention on non-open issue"
                        );
                        if let Some(s) = store {
                            let title = format!(
                                "Skipped @orch command on closed issue #{issue_num} from @{}",
                                mention.author
                            );
                            let task_body = format!(
                                "Mention by @{}:\n\n{}\n\n**Skipped:** target issue is closed",
                                mention.author, mention.body
                            );
                            let _ = s
                                .create_internal(repo, &title, &task_body, "mention", &mention.id)
                                .await;
                        }
                        continue;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(
                            issue = %issue_num,
                            err = %e,
                            "failed to check issue state for slash command in mention"
                        );
                        continue;
                    }
                }

                // Check if author is collaborator
                match gh.is_collaborator(repo, &mention.author).await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::info!(
                            author = %mention.author,
                            command = %command,
                            issue = %issue_num,
                            "ignoring slash command in mention from non-collaborator"
                        );
                        if let Some(s) = store {
                            let title = format!(
                                "Skipped @orch command from non-collaborator @{}",
                                mention.author
                            );
                            let task_body = format!(
                                "Mention by @{}:\n\n{}\n\n**Skipped:** author is not a collaborator",
                                mention.author, mention.body
                            );
                            let _ = s
                                .create_internal(repo, &title, &task_body, "mention", &mention.id)
                                .await;
                        }
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(
                            author = %mention.author,
                            err = %e,
                            "failed to check collaborator status for slash command in mention"
                        );
                        continue;
                    }
                }

                // Execute the command
                let task_id = ExternalId(issue_num.clone());
                let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
                let store_opt = store.cloned();

                match execute_command(
                    backend,
                    &gh,
                    repo,
                    &task_id,
                    &command,
                    &store_opt,
                    task_manager,
                )
                .await
                {
                    Ok(msg) => {
                        tracing::info!(
                            issue = %issue_num,
                            author = %mention.author,
                            command = %command,
                            "executed command from @mention"
                        );
                        let confirmation = format!(
                            "[{now}] {msg} — executed by @{}{}",
                            mention.author,
                            crate::engine::orch_footer()
                        );
                        if let Err(e) = backend.post_comment(&task_id, &confirmation).await {
                            tracing::warn!(issue = %issue_num, err = %e, "failed to post confirmation for mention command");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            issue = %issue_num,
                            command = %command,
                            err = %e,
                            "failed to execute command from @mention"
                        );
                        let error_msg = format!(
                            "[{now}] Failed to execute `{command}`: {e}{}",
                            crate::engine::orch_footer()
                        );
                        if let Err(e2) = backend.post_comment(&task_id, &error_msg).await {
                            tracing::warn!(issue = %issue_num, err = %e2, "failed to post error comment for mention command");
                        }
                    }
                }
                // Mark as processed by creating the task
                if let Some(s) = store {
                    let title = format!(
                        "Respond to mention by @{} — command executed",
                        mention.author
                    );
                    let task_body = format!(
                        "Mention by @{}:\n\n{}\n\n**Status:** Command executed",
                        mention.author, mention.body
                    );
                    if let Err(e) = s
                        .create_internal(repo, &title, &task_body, "mention", &mention.id)
                        .await
                    {
                        tracing::warn!(
                            mention_id = %mention.id,
                            err = %e,
                            "failed to create sentinel for command-executed mention — mention untracked in DB"
                        );
                    }
                }
                // Command was executed (or error-replied) — mention is handled.
                if last_success_ts.as_deref() < Some(mention.created_at.as_str()) {
                    last_success_ts = Some(mention.created_at.clone());
                }
                continue;
            }
        }

        // No command found or command execution failed — create internal task for the mention
        let title = format!("Respond to mention by @{}", mention.author);
        let task_body = format!("Mention by @{}:\n\n{}", mention.author, mention.body);

        if let Some(s) = store {
            match s
                .create_internal(repo, &title, &task_body, "mention", &mention.id)
                .await
            {
                Ok(task_id) => {
                    tracing::info!(task_id, mention_id = %mention.id, "created mention task");
                    if last_success_ts.as_deref() < Some(mention.created_at.as_str()) {
                        last_success_ts = Some(mention.created_at.clone());
                    }
                }
                Err(e) => tracing::warn!(
                    mention_id = %mention.id,
                    err = %e,
                    "failed to create mention task — skipping"
                ),
            }
        }
    }

    // Persist cursor so the next sync tick only fetches newer comments.
    // Use the latest successfully-processed mention timestamp rather than
    // `now`, so any mention whose `create_internal` failed is retried on the
    // next tick instead of being permanently skipped.
    if let Some(ts) = last_success_ts {
        kv_set_prefer_store(&store, "mentions_last_checked", &ts).await;
    }
    // If no mention was successfully processed, leave the cursor unchanged so
    // the entire batch is retried on the next sync tick.

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

/// Inspect router & cooldown state and emit metrics every tick.
///
/// - `metrics:orch.agents_degraded.count` is written on every call with the
///   current number of degraded agents (0 when healthy).
/// - When `count >= 3` a WARN log is emitted with full dimensions (agent
///   names, cooled models, cooldown reasons) and a dedicated alert metric
///   `metrics:orch.agents_degraded.alert` is set to `"1"`. The alert is
///   cleared to `"0"` when the count drops below the threshold.
async fn emit_degraded_agents_if_needed(
    router: &crate::engine::router::Router,
    store: Option<&Arc<TaskStore>>,
) {
    // Complexity tiers to check for configured models.
    const COMPLEXITIES: &[&str] = &["simple", "medium", "complex", "review"];

    let mut details: Vec<DegradedAgentDetail> = Vec::new();

    for agent in &router.available_agents {
        let agent_in_cd = is_agent_in_cooldown(agent);

        // Collect individually cooled models across all complexity tiers (deduped).
        let mut cooled_models: Vec<String> = Vec::new();
        let mut seen_models = std::collections::HashSet::new();
        for comp in COMPLEXITIES {
            if let Some(pool) = router.config.model_pool_for_complexity(agent, comp) {
                for model in pool {
                    if seen_models.insert(model.clone()) && is_model_in_cooldown(agent, &model) {
                        cooled_models.push(model);
                    }
                }
            }
        }

        // An agent is degraded if it is in agent-level cooldown OR has no
        // available (non-cooled) model across any complexity tier.
        let has_model = COMPLEXITIES.iter().any(|comp| {
            router
                .config
                .has_available_model_for_complexity(agent, comp)
        });

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
    kv_set_prefer_store(
        &store,
        "metrics:orch.agents_degraded.count",
        &count.to_string(),
    )
    .await;

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
        kv_set_prefer_store(&store, "metrics:orch.agents_degraded.alert", "1").await;
    } else {
        // Clear the alert metric when healthy.
        kv_set_prefer_store(&store, "metrics:orch.agents_degraded.alert", "0").await;
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

    // Fetch all open issues in one call (includes unlabeled issues).
    // Also fetch routable tasks which catches unlabeled issues on backends
    // where list_all_tasks only returns labeled ones.
    let mut seen = std::collections::HashSet::new();
    let mut all_tasks: Vec<(crate::backends::ExternalTask, Option<Status>)> = Vec::new();

    // 1. Routable tasks (unlabeled + status:new) — these are the ones that
    //    were previously missed because the per-status loop skipped them.
    match backend.list_routable().await {
        Ok(routable) => {
            for task in routable {
                if seen.insert(task.id.0.clone()) {
                    all_tasks.push((task, None));
                }
            }
        }
        Err(e) => {
            tracing::debug!(
                ?e,
                "ingest: failed to list routable tasks — unlabeled new issues may be delayed"
            );
        }
    }

    // 2. All labeled active statuses — fetch in a single API call when backend
    //    supports it, otherwise fall back to per-status calls.
    match backend.list_all_active_tasks().await {
        Ok(active_tasks) => {
            for (task, status) in active_tasks {
                if seen.insert(task.id.0.clone()) {
                    all_tasks.push((task, Some(status)));
                }
            }
        }
        Err(e) => {
            tracing::debug!(
                ?e,
                "ingest: list_all_active_tasks failed, falling back to per-status calls"
            );
            // Fallback: parallel per-status calls for backends that don't
            // implement list_all_active_tasks efficiently.
            let active_statuses = [
                Status::New,
                Status::Routed,
                Status::InProgress,
                Status::NeedsReview,
                Status::InReview,
                Status::Blocked,
            ];
            let status_results = futures::future::join_all(
                active_statuses
                    .iter()
                    .map(|status| async move { (*status, backend.list_by_status(*status).await) }),
            )
            .await;
            for (status, result) in status_results {
                match result {
                    Ok(tasks) => {
                        for task in tasks {
                            if seen.insert(task.id.0.clone()) {
                                all_tasks.push((task, Some(status)));
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!(?status, ?e, "ingest: failed to list tasks");
                    }
                }
            }
        }
    }

    // 3. Upsert into the store, collecting store IDs for batch status check.
    //    Also acknowledge newly detected issues (eyes reaction).
    let mut id_status_pairs: Vec<(i64, crate::backends::Status)> = Vec::new();
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
                            "Duplicate of #{id} — closing to avoid duplicate work.\n\n---\n{footer}",
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
                        if let Err(e) = backend.update_status(&task.id, Status::Done).await {
                            tracing::warn!(
                                task_id = task.id.0,
                                err = %e,
                                "ingest: failed to close duplicate issue"
                            );
                        } else {
                            tracing::info!(
                                task_id = task.id.0,
                                duplicate_of = target.id,
                                "ingest: closed duplicate issue"
                            );
                        }
                        continue;
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
                    let task_status = status.unwrap_or(Status::New);
                    if let Err(e) = backend.sync_to_project(&task.id, task_status).await {
                        tracing::debug!(
                            task_id = task.id.0,
                            err = %e,
                            "ingest: project board sync failed"
                        );
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
        let existing_map = store.get_batch(&all_ids).await.unwrap_or_default();
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{ExternalId, ExternalTask, Mention, Status};
    use crate::engine::cooldown::set_agent_cooldown;
    use crate::store::{RunTokenUsage, TaskStore};
    use async_trait::async_trait;
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

        async fn sync_to_project(&self, id: &ExternalId, status: Status) -> anyhow::Result<()> {
            self.project_syncs.lock().await.push((id.0.clone(), status));
            Ok(())
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

    #[tokio::test]
    async fn ingest_upserts_tasks_into_store() {
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

    #[tokio::test]
    async fn ingest_updates_existing_tasks() {
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

    #[tokio::test]
    async fn ingest_closes_duplicate_titles() {
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

    #[tokio::test]
    async fn ingest_handles_empty_backend() {
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        // Should not error on empty backend
        let result = ingest_external_tasks(&backend, "owner/repo", &store).await;
        assert!(result.is_ok());

        let all = store.list_all("owner/repo").await.unwrap();
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn ingest_syncs_status_correctly_across_statuses() {
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

    #[tokio::test]
    async fn ingest_does_not_overwrite_store_authoritative_status() {
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

    #[tokio::test]
    async fn ingest_sync_to_project_called_only_for_new_tasks() {
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

    // ── kv_get_prefer_store / kv_set_prefer_store ────────────────────

    #[tokio::test]
    async fn kv_get_prefer_store_reads_from_store() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        store.kv_set("k1", "store_val").await.unwrap();

        let opt = Some(&store);
        let val = kv_get_prefer_store(&opt, "k1").await;
        assert_eq!(val.as_deref(), Some("store_val"));
    }

    #[tokio::test]
    async fn kv_get_prefer_store_returns_none_without_store() {
        let opt: Option<&Arc<TaskStore>> = None;
        let val = kv_get_prefer_store(&opt, "k2").await;
        assert_eq!(val, None);
    }

    #[tokio::test]
    async fn kv_set_prefer_store_writes_to_store() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        let opt = Some(&store);
        kv_set_prefer_store(&opt, "k3", "val3").await;

        assert_eq!(store.kv_get("k3").await.unwrap().as_deref(), Some("val3"));
    }

    #[tokio::test]
    async fn kv_set_prefer_store_noop_without_store() {
        let opt: Option<&Arc<TaskStore>> = None;
        // Should not panic
        kv_set_prefer_store(&opt, "k4", "val4").await;
    }

    // ── dispatching lock tests ──────────────────────────────────────────

    #[test]
    fn dispatching_set_blocks_duplicate_processing() {
        let dispatching: Arc<DashSet<String>> = Arc::new(DashSet::new());

        let repo = "owner/repo";
        let task_id = "42";
        let dispatch_key = format!("{}/{}", repo, task_id);

        // Initially not in the set
        assert!(!dispatching.contains(&dispatch_key));

        // Insert — simulates dispatch starting
        dispatching.insert(dispatch_key.clone());

        // Now should be blocked
        assert!(
            dispatching.contains(&dispatch_key),
            "task should be locked while dispatching"
        );

        // Remove — simulates review completion
        dispatching.remove(&dispatch_key);

        // Now should be free again
        assert!(
            !dispatching.contains(&dispatch_key),
            "task should be unlocked after review completes"
        );
    }

    #[test]
    fn dispatching_set_does_not_block_other_tasks() {
        let dispatching: Arc<DashSet<String>> = Arc::new(DashSet::new());

        let repo = "owner/repo";

        // Lock task 42
        let key_42 = format!("{}/42", repo);
        dispatching.insert(key_42.clone());

        // Task 43 should NOT be blocked
        let key_43 = format!("{}/43", repo);
        assert!(dispatching.contains(&key_42), "task 42 should be locked");
        assert!(
            !dispatching.contains(&key_43),
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
    #[tokio::test]
    async fn dispatch_key_held_during_review_agent_execution() {
        use tokio::sync::oneshot;

        let dispatching: Arc<DashSet<String>> = Arc::new(DashSet::new());

        let dispatch_key = "owner/repo/42".to_string();

        // Step 1: insert before spawning — this is the invariant added by a6d8b9a.
        dispatching.insert(dispatch_key.clone());

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
            dispatching.contains(&dispatch_key),
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
            !dispatching.contains(&dispatch_key),
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
    #[tokio::test]
    async fn dispatch_guard_releases_key_on_panic() {
        use crate::engine::dispatch_guard::DispatchGuard;

        let dispatching: Arc<DashSet<String>> = Arc::new(DashSet::new());
        let key = "owner/repo/42".to_string();

        // Insert key before spawn — mirrors the production invariant established
        // by the a6d8b9a fix (key must be visible before the spawn so concurrent
        // review_open_prs callers see it and skip the task).
        dispatching.insert(key.clone());

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
            !dispatching.contains(&key),
            "dispatch key must be removed from the dispatching set even when the \
             spawned review task panics — without DispatchGuard the key leaks and \
             the task gets stuck in a permanent review loop"
        );
    }

    /// An InReview task with no tmux session should be reset to NeedsReview
    /// regardless of review_session_expected — the review agent is dead.
    #[tokio::test]
    async fn stale_in_review_recovery_resets_orphaned_task_without_session() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let router = Arc::new(RwLock::new(crate::engine::router::Router::from_config()));
        let dispatching: Arc<DashSet<String>> = Arc::new(DashSet::new());
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

    #[tokio::test]
    async fn needs_review_refire_increments_and_fires_when_old_enough() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let router = Arc::new(RwLock::new(crate::engine::router::Router::from_config()));
        let dispatching: Arc<DashSet<String>> = Arc::new(DashSet::new());
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

    #[tokio::test]
    async fn needs_review_backoff_delays_fire_but_increments_counter() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let router = Arc::new(RwLock::new(crate::engine::router::Router::from_config()));
        let dispatching: Arc<DashSet<String>> = Arc::new(DashSet::new());
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
        // Counter should have been incremented to 2, but not re-fired (updated_at unchanged)
        assert_eq!(after.needs_review_refires, 2);
        assert_eq!(after.updated_at, now_minus_1);
        assert_eq!(after.status, crate::store::TaskStatus::NeedsReview);
    }

    #[tokio::test]
    async fn needs_review_escalates_to_blocked_after_max_refires() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let router = Arc::new(RwLock::new(crate::engine::router::Router::from_config()));
        let dispatching: Arc<DashSet<String>> = Arc::new(DashSet::new());
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

        // Set counter to MAX-1 (4) so incrementing hits MAX (5) and escalates
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
        assert_eq!(after.status, crate::store::TaskStatus::Blocked);
        assert!(after.block_reason.is_some());
    }

    /// A fresh InReview task (<1 min old) should NOT be reset — the review
    /// agent may still be starting up.
    #[tokio::test]
    async fn stale_in_review_recovery_skips_fresh_tasks() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let router = Arc::new(RwLock::new(crate::engine::router::Router::from_config()));
        let dispatching: Arc<DashSet<String>> = Arc::new(DashSet::new());
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

    #[tokio::test]
    async fn auto_unblock_routes_recoverable_failure() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            store.clone(),
            "owner/repo".to_string(),
        ));
        let dispatching: Arc<DashSet<String>> = Arc::new(DashSet::new());

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

    #[tokio::test]
    async fn auto_unblock_skips_manual_block() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            store.clone(),
            "owner/repo".to_string(),
        ));
        let dispatching: Arc<DashSet<String>> = Arc::new(DashSet::new());

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
        assert_eq!(
            FailureCategory::TokenBudgetExceeded.as_str(),
            "TokenBudgetExceeded"
        );
        assert_eq!(FailureCategory::PushFailed.as_str(), "PushFailed");
        assert_eq!(FailureCategory::PrCreateFailed.as_str(), "PrCreateFailed");
    }

    // ── classify_failure: ModelUnavailable ──────────────────────────────

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

    #[test]
    fn classify_failure_model_does_not_exist_is_recoverable() {
        let category = classify_failure("The model `claude-opus-99` does not exist", "error");
        assert_eq!(category, FailureCategory::ModelUnavailable);
        assert!(category.is_recoverable());
    }

    #[tokio::test]
    async fn auto_unblock_routes_task_blocked_by_model_unavailable() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            store.clone(),
            "owner/repo".to_string(),
        ));
        let dispatching: Arc<DashSet<String>> = Arc::new(DashSet::new());

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

    #[tokio::test]
    async fn auto_unblock_resets_count_when_reason_changes() {
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
        let dispatching: Arc<DashSet<String>> = Arc::new(DashSet::new());

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

    #[tokio::test]
    async fn auto_unblock_stores_reason_on_first_unblock() {
        // When a task is auto-unblocked for the first time, the failure reason
        // should be stored so subsequent blocks can be compared.
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = IngestMockBackend::with_tasks(vec![]);
        let task_manager = Arc::new(TaskManager::with_store(
            backend,
            store.clone(),
            "owner/repo".to_string(),
        ));
        let dispatching: Arc<DashSet<String>> = Arc::new(DashSet::new());

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

    #[test]
    fn classify_failure_sparse_index_not_parse_error() {
        // Another sparse-checkout variant that contains "parse"
        let category = classify_failure("failed due to sparse index configuration", "error");
        assert_ne!(category, FailureCategory::ParseError);
        assert_eq!(category, FailureCategory::Unknown);
    }

    #[test]
    fn dispatching_key_includes_repo_for_cross_project_isolation() {
        let dispatching: Arc<DashSet<String>> = Arc::new(DashSet::new());

        // Lock task 42 in repo A
        let key_a = "owner/repo-a/42".to_string();
        dispatching.insert(key_a.clone());

        // Same task ID in repo B should NOT be blocked
        let key_b = "owner/repo-b/42".to_string();
        assert!(dispatching.contains(&key_a));
        assert!(
            !dispatching.contains(&key_b),
            "same task ID in different repo should not be blocked"
        );
    }

    #[tokio::test]
    async fn emit_degraded_agents_writes_metric_when_three_or_more_agents_degraded() {
        use crate::engine::router::{Router, RouterConfig};

        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        // Create a router and set deterministic available agents
        let mut router = Router::new(RouterConfig::default());
        router.available_agents = vec![
            "test-agent-3a".to_string(),
            "test-agent-3b".to_string(),
            "test-agent-3c".to_string(),
            "test-agent-3d".to_string(),
        ];

        // Place three agents into cooldown (in-memory only)
        set_agent_cooldown("test-agent-3a", 3600);
        set_agent_cooldown("test-agent-3b", 3600);
        set_agent_cooldown("test-agent-3c", 3600);

        // Call the helper and assert KV metrics written
        emit_degraded_agents_if_needed(&router, Some(&store)).await;

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

    #[tokio::test]
    async fn emit_degraded_agents_clears_alert_when_below_threshold() {
        use crate::engine::router::{Router, RouterConfig};

        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        let mut router = Router::new(RouterConfig::default());
        router.available_agents = vec!["test-agent-2a".to_string(), "test-agent-2b".to_string()];

        // Place only two agents into cooldown — below the threshold of 3
        set_agent_cooldown("test-agent-2a", 3600);
        set_agent_cooldown("test-agent-2b", 3600);

        emit_degraded_agents_if_needed(&router, Some(&store)).await;

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
    }
}
