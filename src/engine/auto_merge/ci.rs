use crate::engine::review_gate;
use crate::github::http::GhHttp;
use crate::github::types::{GitHubCheckRun, RequiredStatusCheck};
use crate::security;
use crate::store::TaskStore;
use crate::store::store_increment;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;

/// Record that CI was checked for this task, updating the timestamp in KV store.
pub(super) async fn record_ci_check(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
) -> anyhow::Result<()> {
    let key = ci_check_ts_key(repo, task_id);
    let now = chrono::Utc::now().to_rfc3339();
    store.kv_set(&key, &now).await
}

pub(super) fn ci_check_ts_key(repo: &str, task_id: &str) -> String {
    format!("ci_check_ts:{repo}#{task_id}")
}

/// Prefix shared by the pending timers of every head of one task
pub(super) fn ci_pending_since_prefix(repo: &str, task_id: &str) -> String {
    format!("ci_pending_since:{repo}#{task_id}#")
}

/// The head sha in the key means a new head starts a new pending window
pub(super) fn ci_pending_since_key(repo: &str, task_id: &str, head_sha: &str) -> String {
    format!("{}{head_sha}", ci_pending_since_prefix(repo, task_id))
}

/// Prefix shared by the counted-failure keys of every head of one task
pub(super) fn ci_failure_sha_prefix(repo: &str, task_id: &str) -> String {
    format!("ci_failure_sha:{repo}#{task_id}#")
}

pub(super) fn ci_failure_sha_key(repo: &str, task_id: &str, head_sha: &str) -> String {
    format!("{}{head_sha}", ci_failure_sha_prefix(repo, task_id))
}

/// Increment `ci_merge_failures` once per head SHA. `None` when this SHA was already counted
pub(crate) async fn count_ci_failure_for_head(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    head_sha: &str,
) -> anyhow::Result<Option<u64>> {
    let key = (!head_sha.is_empty()).then(|| ci_failure_sha_key(repo, task_id, head_sha));
    if let Some(ref key) = key
        && store.kv_increment(key).await? > 1
    {
        return Ok(None);
    }
    match store_increment(&Some(Arc::clone(store)), repo, task_id, "ci_merge_failures").await {
        Ok(n) => Ok(Some(n)),
        Err(e) => {
            if let Some(ref key) = key
                && let Err(del) = store.kv_delete(key).await
            {
                tracing::warn!(
                    task_id,
                    err = %del,
                    "failed to roll back ci_failure_sha after increment error"
                );
            }
            Err(e)
        }
    }
}

/// Return how long CI has been continuously observed as `pending` for this
/// task, persisting the first-seen timestamp in the KV store.
///
/// `auto_merge_pr` runs as many short-lived invocations rather than one long
/// blocking call — `is_ci_check_in_cooldown` returns early well before
/// `max_wait` elapses, and the next sync tick spawns a fresh invocation with
/// its own local `Instant`. A local `start.elapsed()` therefore never
/// accumulates past a few seconds, so the pending-timeout escalation can
/// never fire. Persisting "first seen pending" here lets elapsed time
/// accumulate across those invocations the same way `ci_check_ts` already
/// does for the check cooldown.
pub(super) async fn ci_pending_elapsed(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    head_sha: &str,
) -> anyhow::Result<chrono::Duration> {
    let key = ci_pending_since_key(repo, task_id, head_sha);
    let now = chrono::Utc::now();
    // `auto_merge_pr` can run as two overlapping invocations for the same task
    // (the review-agent's direct approval call in review.rs races the periodic
    // review_open_prs sweep, which is not guarded against that call site). A
    // plain "read, then write-if-absent" has a TOCTOU window where both
    // invocations observe an absent key and each writes its own "now",
    // silently discarding whichever wrote first and resetting the elapsed
    // clock. `kv_insert_if_absent` performs the check-and-set in one SQL
    // statement so only the first writer's timestamp survives.
    let stored = store.kv_insert_if_absent(&key, &now.to_rfc3339()).await?;
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&stored) {
        Ok(now.signed_duration_since(dt.with_timezone(&chrono::Utc)))
    } else {
        tracing::warn!(
            task_id,
            stored,
            "ci_pending_since value unparseable — resetting"
        );
        store.kv_set(&key, &now.to_rfc3339()).await?;
        Ok(chrono::Duration::zero())
    }
}

/// Clear the persisted pending-since timestamp once CI leaves the pending
/// state (success, failure, or after a timeout escalation fires) so the next
/// pending streak starts fresh.
///
/// `reason` identifies which branch triggered the clear (e.g. "ci_success",
/// "ci_failure", "pending_timeout_escalation") and is logged alongside the
/// value being cleared, so a future occurrence of the elapsed time silently
/// resetting (see #3609) is diagnosable from logs alone instead of requiring
/// another multi-day live reproduction.
pub(super) async fn clear_ci_pending_since(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    reason: &str,
) -> anyhow::Result<()> {
    for (key, prior) in store
        .kv_list_prefix(&ci_pending_since_prefix(repo, task_id))
        .await?
    {
        tracing::debug!(task_id, reason, prior_since = %prior, "clearing ci_pending_since");
        store.kv_delete(&key).await?;
    }
    Ok(())
}

/// Delete the CI timers of tasks that finished, and the bare-id keys older versions wrote
pub(crate) async fn sweep_ci_timer_keys(
    store: &Arc<TaskStore>,
    repo: &str,
    finished_task_ids: &[String],
) -> anyhow::Result<()> {
    let finished: HashSet<&str> = finished_task_ids.iter().map(String::as_str).collect();
    let repo_marker = format!("{repo}#");
    for prefix in [
        "ci_check_ts:",
        "ci_pending_since:",
        "review_ci_wait_since:",
        "ci_failure_sha:",
        "ci_bootstrap_wait_since:",
        "ci_bootstrap_no_checks_comment:",
        "ci_bootstrap_never_run_comment:",
    ] {
        for (key, _) in store.kv_list_prefix(prefix).await? {
            let rest = &key[prefix.len()..];
            let stale = match rest.strip_prefix(repo_marker.as_str()) {
                // Pending keys carry a trailing `#{head_sha}`
                Some(tail) => finished.contains(tail.split('#').next().unwrap_or(tail)),
                None => !rest.contains('#'),
            };
            if stale {
                store.kv_delete(&key).await?;
            }
        }
    }
    Ok(())
}

pub(crate) fn required_checks_state(
    required_contexts: &[RequiredStatusCheck],
    check_runs: &[GitHubCheckRun],
    statuses: &[(String, String)],
) -> (String, u64, u64, u64, u64) {
    if required_contexts.is_empty() {
        return ("success".to_string(), 0, 0, 0, 0);
    }

    let mut passing = 0u64;
    let mut failing = 0u64;
    let mut pending = 0u64;

    for required in required_contexts {
        let mut matched = false;

        if let Some(run) = check_runs
            .iter()
            .find(|run| required_check_run_matches(required, run))
        {
            matched = true;
            if !run.is_finished() {
                pending += 1;
            } else if run.is_passed() {
                passing += 1;
            } else {
                failing += 1;
            }
        } else {
            for (name, state) in statuses {
                if !required_commit_status_matches(required, name) {
                    continue;
                }
                matched = true;
                match state.as_str() {
                    "success" | "neutral" | "skipped" => passing += 1,
                    "failure" | "error" => failing += 1,
                    _ => pending += 1,
                }
                break;
            }
        }

        if !matched {
            pending += 1;
        }
    }

    let total = required_contexts.len() as u64;
    let state = if failing > 0 {
        "failure".to_string()
    } else if pending > 0 {
        "pending".to_string()
    } else {
        "success".to_string()
    };

    (state, total, passing, failing, pending)
}

pub(crate) fn required_check_run_matches(
    required: &RequiredStatusCheck,
    run: &GitHubCheckRun,
) -> bool {
    if run.name != required.context {
        return false;
    }
    match required.integration_id {
        None => true,
        Some(id) => run.app.as_ref().is_some_and(|app| app.id == id),
    }
}

pub(crate) fn required_commit_status_matches(
    required: &RequiredStatusCheck,
    context: &str,
) -> bool {
    required.integration_id.is_none() && required.context == context
}

pub(super) const CI_LOG_TAIL_LINES: usize = 80;

pub(super) const CI_CONTEXT_MAX_CHARS: usize = 8000;

/// The `pr_review_context` for a task routed back on red CI (#4121), empty `failed` means all
pub(crate) async fn red_ci_context(
    gh: &GhHttp,
    repo: &str,
    pr_number: u64,
    sha: &str,
    failed: &[String],
) -> String {
    let mut sections: Vec<(String, String)> = Vec::new();
    match gh.get_check_runs(repo, sha).await {
        Ok(runs) => {
            let failed_runs = runs.iter().filter(|r| {
                matches!(r.conclusion.as_deref(), Some("failure" | "timed_out"))
                    && (failed.is_empty() || failed.contains(&r.name))
            });
            for run in failed_runs {
                sections.push((run.name.clone(), failure_text(gh, repo, run).await));
            }
        }
        Err(e) => {
            tracing::debug!(err = %e, "failed to fetch check runs for the CI failure context");
        }
    }
    for name in failed {
        if !sections.iter().any(|(n, _)| n == name) {
            sections.push((name.clone(), String::new()));
        }
    }
    format_red_ci_context(pr_number, sha, &sections)
}

/// Annotations of a failed check run, or the tail of its job log when it has none
pub(super) async fn failure_text(gh: &GhHttp, repo: &str, run: &GitHubCheckRun) -> String {
    match gh.get_check_run_annotations(repo, run.id).await {
        Ok(annotations) => {
            let lines: Vec<String> = annotations
                .iter()
                .filter(|a| a.annotation_level.as_deref() != Some("notice"))
                .map(|a| {
                    let place = match (&a.path, a.start_line) {
                        (Some(path), Some(line)) => format!("{path}:{line}: "),
                        (Some(path), None) => format!("{path}: "),
                        _ => String::new(),
                    };
                    let title = a.title.as_deref().unwrap_or("");
                    let message = a.message.as_deref().unwrap_or("");
                    format!("{place}{title} {message}").trim().to_string()
                })
                .filter(|l| !l.is_empty())
                .collect();
            if !lines.is_empty() {
                return lines.join("\n");
            }
        }
        Err(e) => {
            tracing::debug!(check_run_id = run.id, err = %e, "failed to fetch check run annotations");
        }
    }
    gh.get_job_log_tail(repo, run.id, CI_LOG_TAIL_LINES)
        .await
        .unwrap_or_default()
}

pub(crate) fn format_red_ci_context(
    pr_number: u64,
    sha: &str,
    sections: &[(String, String)],
) -> String {
    let names = sections
        .iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = format!(
        "CI failed on PR #{pr_number} at head {sha}. Failed checks: {names}.\n\
         Run each failing check locally and fix it. Do not report done until it passes.\n"
    );
    for (name, text) in sections {
        let text = text.trim();
        let text = if text.is_empty() {
            "No failure text was available. Read the log of this check on the PR."
        } else {
            text
        };
        let _ = write!(out, "\n### {name}\n```\n{text}\n```\n");
    }
    let mut out = security::redact(&out);
    if out.chars().count() > CI_CONTEXT_MAX_CHARS {
        out = out.chars().take(CI_CONTEXT_MAX_CHARS).collect();
        out.push_str("\n[truncated]");
    }
    out
}

/// Counts failed check runs for `sha` that carry a GitHub Actions billing
/// failure annotation.
///
/// When an account has unpaid invoices or has hit a spending limit, GitHub
/// refuses to start jobs and records the reason ("The job was not started
/// because recent account payments have failed …") as a check-run annotation.
/// The CI state still comes back as `"failure"`, which would normally trigger
/// the agent retry loop — but no code change can fix a billing problem.
/// This function lets the caller short-circuit that loop and block the task
/// immediately with a human-readable billing message instead.
pub(crate) async fn billing_failed_runs(
    gh: &GhHttp,
    repo: &str,
    sha: &str,
    prefetched: Option<Vec<GitHubCheckRun>>,
) -> u64 {
    let check_runs = match prefetched {
        Some(runs) => runs,
        None => match gh.get_check_runs(repo, sha).await {
            Ok(runs) => runs,
            Err(e) => {
                tracing::debug!(err = %e, "failed to fetch check runs for billing failure check");
                return 0;
            }
        },
    };

    let mut count = 0;
    for run in check_runs
        .iter()
        .filter(|r| r.conclusion.as_deref() == Some("failure"))
    {
        match gh.get_check_run_annotations(repo, run.id).await {
            Ok(annotations) => {
                for annotation in &annotations {
                    let msg = annotation.message.as_deref().unwrap_or("");
                    let title = annotation.title.as_deref().unwrap_or("");
                    if msg.contains("account payments have failed")
                        || msg.contains("spending limit")
                        || title.contains("account payments have failed")
                        || title.contains("spending limit")
                    {
                        count += 1;
                        break;
                    }
                }
            }
            Err(e) => {
                tracing::debug!(
                    check_run_id = run.id,
                    err = %e,
                    "failed to fetch check run annotations"
                );
            }
        }
    }

    count
}

/// Whether a PR head may merge, given how many commits it is behind its base.
///
/// `None` means the comparison could not be read. Only a head that contains the
/// base tip has run its checks against the code the merge would produce, so an
/// unreadable comparison holds the merge too (#3780)
pub(super) fn may_merge_onto_base(behind_by: Option<u64>) -> bool {
    behind_by == Some(0)
}

/// True when any check run or commit status has not finished
pub(super) fn any_check_pending(
    check_runs: &[GitHubCheckRun],
    statuses: &[(String, String)],
) -> bool {
    check_runs
        .iter()
        .any(|run| !review_gate::is_own_check(&run.name) && !run.is_finished())
        || statuses.iter().any(|(name, state)| {
            !review_gate::is_own_check(name)
                && !matches!(
                    state.as_str(),
                    "success" | "failure" | "error" | "neutral" | "skipped"
                )
        })
}

/// Names of completed checks that failed and are not required
pub(super) fn failing_optional_check_names(
    required: &[RequiredStatusCheck],
    check_runs: &[GitHubCheckRun],
    statuses: &[(String, String)],
) -> Vec<String> {
    let mut names = Vec::new();
    for run in check_runs {
        if required.iter().any(|r| required_check_run_matches(r, run)) {
            continue;
        }
        if run.is_finished() && !run.is_passed() {
            names.push(run.name.clone());
        }
    }
    for (name, state) in statuses {
        if !matches!(state.as_str(), "failure" | "error") {
            continue;
        }
        if required
            .iter()
            .any(|r| required_commit_status_matches(r, name))
        {
            continue;
        }
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    names
}

/// `has_hooks` is `clean` on a server with pre-receive hooks
/// `unstable` is ready when required checks passed, and waits while a repo with no required checks still has running CI
pub(super) fn mergeable_state_ready(
    state: &str,
    required: &[RequiredStatusCheck],
    check_runs: &[GitHubCheckRun],
    statuses: &[(String, String)],
) -> bool {
    match state {
        "clean" | "has_hooks" => true,
        "unstable" => {
            if required.is_empty() {
                !any_check_pending(check_runs, statuses)
            } else {
                required_checks_state(required, check_runs, statuses).0 == "success"
            }
        }
        _ => false,
    }
}

/// Check whether all *required* CI checks pass for the given PR.
///
/// Returns `Some(true)` when all required contexts report success, `Some(false)` when
/// at least one required check is failing or pending, and `None` on any API error (so
/// the caller can fall through to the default escalation path).
pub(super) async fn required_ci_checks_pass(
    gh: &GhHttp,
    repo: &str,
    pr_number: u64,
) -> Option<bool> {
    let pr = match gh.get_pr(repo, pr_number).await {
        Ok(pr) => pr,
        Err(e) => {
            tracing::warn!(pr_number, err = %e, "required_ci_checks_pass: failed to fetch PR");
            return None;
        }
    };

    let head_sha = pr.head.sha.clone();
    let base_branch = pr.base.ref_.clone();

    let required_contexts = match gh
        .get_required_status_check_contexts(repo, &base_branch)
        .await
    {
        Ok(ctx) => ctx,
        Err(e) => {
            tracing::warn!(
                pr_number,
                err = %e,
                "required_ci_checks_pass: failed to fetch required contexts"
            );
            return None;
        }
    };

    // No required contexts configured — treat as passing.
    if required_contexts.is_empty() {
        return Some(true);
    }

    let check_runs = match gh.get_check_runs(repo, &head_sha).await {
        Ok(runs) => runs,
        Err(e) => {
            tracing::warn!(
                pr_number,
                err = %e,
                "required_ci_checks_pass: failed to fetch check runs"
            );
            return None;
        }
    };

    let statuses = match gh.get_commit_status_contexts(repo, &head_sha).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                pr_number,
                err = %e,
                "required_ci_checks_pass: failed to fetch commit statuses"
            );
            return None;
        }
    };

    let (state, ..) = required_checks_state(&required_contexts, &check_runs, &statuses);
    Some(state == "success")
}
