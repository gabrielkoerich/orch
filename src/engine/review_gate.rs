//! The CI gate before a review: a review starts only once CI has run on the PR head

use crate::github::http::GhHttp;
use crate::github::types::{GitHubCheckRun, RequiredStatusCheck};
use crate::store::TaskStore;
use std::sync::Arc;

/// Checks that report the review itself, so they never hold one back
const REVIEW_OWN_CHECKS: &[&str] = &["review-gate", "orch/review"];

/// How long a PR head may show no started checks before the review starts anyway
const NO_CHECKS_WAIT_SECS: i64 = 15 * 60;

#[derive(Debug, PartialEq)]
pub(crate) enum CiGate {
    /// Start the review now
    Start,
    /// CI is still running on the PR head
    Wait,
    /// CI failed on the PR head, these checks failed
    Red(Vec<String>),
}

#[derive(Debug, PartialEq)]
enum CiState {
    /// No check has started on the head yet, or every run is still queued
    NotStarted,
    Running,
    Green,
    Red(Vec<String>),
}

/// Decide whether the review of `pr_number` may start
pub(crate) async fn review_ci_gate(
    gh: &GhHttp,
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    pr_number: u64,
) -> anyhow::Result<CiGate> {
    let pr = gh.get_pr(repo, pr_number).await?;
    let sha = pr.head.sha;
    let required: Vec<RequiredStatusCheck> = gh
        .get_required_status_check_contexts(repo, &pr.base.ref_)
        .await?
        .into_iter()
        .filter(|r| !is_own_check(&r.context))
        .collect();
    if required.is_empty() && !gh.has_workflows(repo).await? {
        return Ok(CiGate::Start);
    }

    let runs = gh.get_check_runs(repo, &sha).await?;
    let statuses = gh.get_commit_status_contexts(repo, &sha).await?;
    let wait_key = format!("review_ci_wait_since:{repo}#{task_id}#{sha}");
    let state = match ci_state(&required, &runs, &statuses) {
        CiState::Red(failed) => {
            // A check that never started for an Actions billing limit did not run, it did not fail
            let unstarted =
                crate::engine::auto_merge::billing_failed_runs(gh, repo, &sha, Some(runs)).await;
            if unstarted as usize >= failed.len() {
                CiState::NotStarted
            } else {
                CiState::Red(failed)
            }
        }
        state => state,
    };

    let gate = match state {
        CiState::Green => CiGate::Start,
        CiState::Running => CiGate::Wait,
        CiState::Red(failed) => CiGate::Red(failed),
        CiState::NotStarted => {
            let now = chrono::Utc::now().timestamp();
            let since = match store.kv_get(&wait_key).await? {
                Some(v) => v.parse().unwrap_or(now),
                None => {
                    store.kv_set(&wait_key, &now.to_string()).await?;
                    now
                }
            };
            if now - since >= NO_CHECKS_WAIT_SECS {
                tracing::warn!(
                    task_id,
                    pr_number,
                    "no CI ran on the PR head for 15 minutes, starting the review anyway"
                );
                CiGate::Start
            } else {
                CiGate::Wait
            }
        }
    };
    if gate != CiGate::Wait {
        store.kv_delete(&wait_key).await.ok();
    }

    Ok(gate)
}

fn is_own_check(name: &str) -> bool {
    REVIEW_OWN_CHECKS.contains(&name)
}

/// Every check on the head counts, or only the required ones when the base branch has any
fn ci_state(
    required: &[RequiredStatusCheck],
    runs: &[GitHubCheckRun],
    statuses: &[(String, String)],
) -> CiState {
    // (name, started, finished, passed)
    let mut checks: Vec<(String, bool, bool, bool)> = Vec::new();
    for run in runs.iter().filter(|r| !is_own_check(&r.name)) {
        let finished = run.status == "completed" && run.conclusion.is_some();
        let passed = matches!(
            run.conclusion.as_deref(),
            Some("success" | "neutral" | "skipped")
        );
        checks.push((run.name.clone(), run.status != "queued", finished, passed));
    }
    for (context, state) in statuses.iter().filter(|(c, _)| !is_own_check(c)) {
        let finished = state != "pending";
        let passed = matches!(state.as_str(), "success" | "neutral" | "skipped");
        checks.push((context.clone(), true, finished, passed));
    }
    if !required.is_empty() {
        checks.retain(|(name, ..)| required.iter().any(|r| &r.context == name));
    }

    if checks.iter().all(|(_, started, ..)| !started) {
        return CiState::NotStarted;
    }

    let failed: Vec<String> = checks
        .iter()
        .filter(|(_, _, finished, passed)| *finished && !passed)
        .map(|(name, ..)| name.clone())
        .collect();
    if !failed.is_empty() {
        CiState::Red(failed)
    } else if checks.iter().any(|(_, _, finished, _)| !finished)
        || required
            .iter()
            .any(|r| !checks.iter().any(|(name, ..)| name == &r.context))
    {
        CiState::Running
    } else {
        CiState::Green
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str, status: &str, conclusion: Option<&str>) -> GitHubCheckRun {
        GitHubCheckRun {
            id: 1,
            name: name.to_string(),
            status: status.to_string(),
            conclusion: conclusion.map(String::from),
            app: None,
        }
    }

    fn required(name: &str) -> RequiredStatusCheck {
        RequiredStatusCheck {
            context: name.to_string(),
            integration_id: None,
        }
    }

    #[test]
    fn no_checks_or_only_queued_ones_have_not_started() {
        assert_eq!(ci_state(&[], &[], &[]), CiState::NotStarted);
        assert_eq!(
            ci_state(&[], &[run("test", "queued", None)], &[]),
            CiState::NotStarted
        );
    }

    #[test]
    fn a_running_check_waits_and_finished_ones_decide() {
        let running = [
            run("test", "in_progress", None),
            run("lint", "completed", Some("success")),
        ];
        assert_eq!(ci_state(&[], &running, &[]), CiState::Running);

        let green = [
            run("test", "completed", Some("success")),
            run("docs", "completed", Some("skipped")),
        ];
        assert_eq!(ci_state(&[], &green, &[]), CiState::Green);

        let red = [
            run("test", "completed", Some("failure")),
            run("lint", "in_progress", None),
        ];
        assert_eq!(
            ci_state(&[], &red, &[]),
            CiState::Red(vec!["test".to_string()])
        );
    }

    #[test]
    fn the_review_checks_never_hold_the_review() {
        let runs = [
            run("review-gate", "completed", Some("failure")),
            run("orch/review", "in_progress", None),
            run("test", "completed", Some("success")),
        ];
        assert_eq!(ci_state(&[], &runs, &[]), CiState::Green);
        let statuses = [("orch/review".to_string(), "failure".to_string())];
        assert_eq!(ci_state(&[], &[], &statuses), CiState::NotStarted);
    }

    #[test]
    fn only_required_checks_count_when_the_base_has_any() {
        let runs = [
            run("test", "completed", Some("success")),
            run("optional", "completed", Some("failure")),
        ];
        assert_eq!(ci_state(&[required("test")], &runs, &[]), CiState::Green);
        let statuses = [("ci/legacy".to_string(), "failure".to_string())];
        assert_eq!(
            ci_state(&[required("ci/legacy")], &runs, &statuses),
            CiState::Red(vec!["ci/legacy".to_string()])
        );
    }

    #[test]
    fn a_required_check_that_has_not_reported_keeps_the_review_waiting() {
        let runs = [run("test", "completed", Some("success"))];
        let both = [required("test"), required("lint")];
        assert_eq!(ci_state(&both, &runs, &[]), CiState::Running);
        assert_eq!(ci_state(&both, &[], &[]), CiState::NotStarted);
        let failed = [run("test", "completed", Some("failure"))];
        assert_eq!(
            ci_state(&both, &failed, &[]),
            CiState::Red(vec!["test".to_string()])
        );
    }

    #[test]
    fn a_pending_commit_status_keeps_the_review_waiting() {
        let statuses = [("ci/build".to_string(), "pending".to_string())];
        assert_eq!(ci_state(&[], &[], &statuses), CiState::Running);
    }
}
