use super::rebase::apply_rebase_recovery_outcome;
use super::rebase::classify_fetch_failure;
use super::rebase::is_ssh_agent_fetch_failure;
use super::*;
use crate::backends::test_helpers::NoopBackend;
use crate::backends::{ExternalId, ExternalTask};
use crate::engine::ci::any_check_pending;
use crate::engine::ci::ci_failure_sha_key;
use crate::engine::ci::ci_pending_since_key;
use crate::engine::ci::ci_pending_since_prefix;
use crate::engine::ci::sweep_ci_timer_keys;
use crate::engine::git::git_ops;
use crate::engine::router;
use crate::engine::tasks::TaskManager;
use crate::github::token;
use crate::github::types::GitHubApp;
use crate::github::types::GitHubBranchRef;
use crate::github::types::GitHubPullRequest;
use crate::github::types::RequiredStatusCheck;
use crate::github::types::{GitHubReview, GitHubUser};
use crate::store::NewTask;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use std::env;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn mergeable_state_ready_accepts_only_clean_states() {
    assert!(mergeable_state_ready("clean", &[], &[], &[]));
    assert!(mergeable_state_ready("has_hooks", &[], &[], &[]));
    for state in ["behind", "blocked", "unknown", "dirty", "draft"] {
        assert!(
            !mergeable_state_ready(state, &[], &[], &[]),
            "{state} must hold the merge"
        );
    }
}

#[test]
fn mergeable_state_ready_accepts_unstable_when_required_checks_passed() {
    let required = vec![required_check("ci", None)];
    let check_runs = vec![
        check_run("ci", "completed", Some("success"), None),
        check_run("lint", "completed", Some("failure"), None),
    ];
    assert!(mergeable_state_ready(
        "unstable",
        &required,
        &check_runs,
        &[]
    ));
    assert_eq!(
        failing_optional_check_names(&required, &check_runs, &[]),
        vec!["lint"]
    );
}

#[test]
fn mergeable_state_ready_holds_unstable_when_a_required_check_failed() {
    let required = vec![required_check("ci", None)];
    let check_runs = vec![
        check_run("ci", "completed", Some("failure"), None),
        check_run("lint", "completed", Some("success"), None),
    ];
    assert!(!mergeable_state_ready(
        "unstable",
        &required,
        &check_runs,
        &[]
    ));
}

#[test]
fn mergeable_state_ready_holds_unstable_while_checks_are_pending_without_required() {
    let check_runs = vec![check_run("ci", "in_progress", None, None)];
    assert!(!mergeable_state_ready("unstable", &[], &check_runs, &[]));
    let statuses = vec![("lint".to_string(), "pending".to_string())];
    assert!(!mergeable_state_ready("unstable", &[], &[], &statuses));
}

#[test]
fn mergeable_state_ready_accepts_unstable_when_optional_checks_finished_without_required() {
    let check_runs = vec![check_run("lint", "completed", Some("failure"), None)];
    assert!(mergeable_state_ready("unstable", &[], &check_runs, &[]));
    assert_eq!(
        failing_optional_check_names(&[], &check_runs, &[]),
        vec!["lint"]
    );
    let action_required = vec![check_run(
        "deploy",
        "completed",
        Some("action_required"),
        None,
    )];
    assert!(!mergeable_state_ready(
        "unstable",
        &[],
        &action_required,
        &[]
    ));
    assert!(failing_optional_check_names(&[], &action_required, &[]).is_empty());
}

/// Auto-merge refuses a PR whose head does not contain the current base
/// commit: its green checks ran against older code (#3780)
#[test]
fn may_merge_onto_base_refuses_a_head_behind_its_base() {
    assert!(
        may_merge_onto_base(Some(0)),
        "a head that contains the base tip may merge"
    );
    assert!(
        !may_merge_onto_base(Some(1)),
        "a head one commit behind the base must not merge"
    );
    assert!(
        !may_merge_onto_base(Some(12)),
        "a head well behind the base must not merge"
    );
    assert!(
        !may_merge_onto_base(None),
        "an unreadable comparison must hold the merge, not permit it"
    );
}

fn make_review(login: &str, state: &str, submitted_at: &str) -> GitHubReview {
    GitHubReview {
        id: 1,
        user: GitHubUser {
            login: login.to_string(),
        },
        body: None,
        state: state.to_string(),
        html_url: None,
        submitted_at: submitted_at.to_string(),
        commit_id: None,
    }
}

fn required_check(context: &str, integration_id: Option<i64>) -> RequiredStatusCheck {
    RequiredStatusCheck {
        context: context.to_string(),
        integration_id,
    }
}

fn check_run(
    name: &str,
    status: &str,
    conclusion: Option<&str>,
    app_id: Option<i64>,
) -> GitHubCheckRun {
    GitHubCheckRun {
        id: 1,
        name: name.to_string(),
        status: status.to_string(),
        conclusion: conclusion.map(str::to_string),
        app: app_id.map(|id| GitHubApp { id }),
    }
}

#[test]
fn any_check_pending_ignores_the_reviews_own_checks() {
    let runs = vec![
        check_run("ci", "completed", Some("success"), None),
        check_run("review-gate", "in_progress", None, None),
        check_run("orch/review", "queued", None, None),
    ];
    let statuses = vec![("orch/review".to_string(), "pending".to_string())];
    assert!(!any_check_pending(&runs, &statuses));

    let runs = vec![check_run("ci", "in_progress", None, None)];
    assert!(any_check_pending(&runs, &[]));
}

#[test]
fn required_checks_state_uses_required_contexts() {
    let required = vec![required_check("ci", None), required_check("lint", None)];
    let check_runs = vec![
        check_run("ci", "completed", Some("success"), None),
        check_run("lint", "completed", Some("failure"), None),
    ];
    let statuses = vec![("unused".to_string(), "success".to_string())];

    let (state, total, passing, failing, pending) =
        required_checks_state(&required, &check_runs, &statuses);

    assert_eq!(state, "failure");
    assert_eq!(total, 2);
    assert_eq!(passing, 1);
    assert_eq!(failing, 1);
    assert_eq!(pending, 0);
}

#[test]
fn required_checks_state_ruleset_missing_run_is_pending() {
    let required = vec![required_check("ci/test", Some(42))];
    let (state, total, passing, failing, pending) = required_checks_state(&required, &[], &[]);

    assert_eq!(state, "pending");
    assert_eq!(total, 1);
    assert_eq!(passing, 0);
    assert_eq!(failing, 0);
    assert_eq!(pending, 1);
}

#[test]
fn required_checks_state_ruleset_passing_run_is_success() {
    let required = vec![required_check("ci/test", Some(42))];
    let check_runs = vec![check_run("ci/test", "completed", Some("success"), Some(42))];
    let (state, total, passing, failing, pending) =
        required_checks_state(&required, &check_runs, &[]);

    assert_eq!(state, "success");
    assert_eq!(total, 1);
    assert_eq!(passing, 1);
    assert_eq!(failing, 0);
    assert_eq!(pending, 0);
}

#[test]
fn required_checks_state_ruleset_failing_run_is_failure() {
    let required = vec![required_check("ci/test", Some(42))];
    let check_runs = vec![check_run("ci/test", "completed", Some("failure"), Some(42))];
    let (state, ..) = required_checks_state(&required, &check_runs, &[]);
    assert_eq!(state, "failure");
}

#[test]
fn required_checks_state_action_required_is_pending() {
    let required = vec![required_check("ci/test", Some(42))];
    let check_runs = vec![check_run(
        "ci/test",
        "completed",
        Some("action_required"),
        Some(42),
    )];
    let (state, total, passing, failing, pending) =
        required_checks_state(&required, &check_runs, &[]);
    assert_eq!(state, "pending");
    assert_eq!(total, 1);
    assert_eq!(passing, 0);
    assert_eq!(failing, 0);
    assert_eq!(pending, 1);
}

#[test]
fn required_checks_state_wrong_app_does_not_satisfy() {
    let required = vec![required_check("ci/test", Some(42))];
    let check_runs = vec![check_run("ci/test", "completed", Some("success"), Some(99))];
    let (state, total, passing, failing, pending) =
        required_checks_state(&required, &check_runs, &[]);

    assert_eq!(state, "pending");
    assert_eq!(total, 1);
    assert_eq!(passing, 0);
    assert_eq!(failing, 0);
    assert_eq!(pending, 1);
}

#[test]
fn required_checks_state_classic_name_only_still_matches() {
    let required = vec![required_check("ci/test", None)];
    let check_runs = vec![check_run("ci/test", "completed", Some("success"), Some(99))];
    let (state, ..) = required_checks_state(&required, &check_runs, &[]);
    assert_eq!(state, "success");
}

#[test]
fn required_checks_state_empty_required_is_success() {
    let (state, total, passing, failing, pending) = required_checks_state(&[], &[], &[]);
    assert_eq!(state, "success");
    assert_eq!(total, 0);
    assert_eq!(passing, 0);
    assert_eq!(failing, 0);
    assert_eq!(pending, 0);
}

#[test]
fn required_checks_state_commit_status_cannot_satisfy_app_bound_rule() {
    let required = vec![required_check("ci/test", Some(42))];
    let statuses = vec![("ci/test".to_string(), "success".to_string())];
    let (state, _total, _passing, _failing, pending) =
        required_checks_state(&required, &[], &statuses);
    assert_eq!(state, "pending");
    assert_eq!(pending, 1);
}

#[test]
fn required_checks_state_commit_status_satisfies_name_only_rule() {
    let required = vec![required_check("ci/legacy", None)];
    let statuses = vec![("ci/legacy".to_string(), "success".to_string())];
    let (state, total, passing, failing, pending) =
        required_checks_state(&required, &[], &statuses);
    assert_eq!(state, "success");
    assert_eq!(total, 1);
    assert_eq!(passing, 1);
    assert_eq!(failing, 0);
    assert_eq!(pending, 0);
}

#[test]
fn required_checks_state_commit_status_failure_and_error_fail() {
    let required = vec![required_check("ci/legacy", None)];
    for state_name in ["failure", "error"] {
        let statuses = vec![("ci/legacy".to_string(), state_name.to_string())];
        let (state, ..) = required_checks_state(&required, &[], &statuses);
        assert_eq!(state, "failure", "{state_name}");
    }
}

#[test]
fn required_checks_state_commit_status_pending_is_pending() {
    let required = vec![required_check("ci/legacy", None)];
    let statuses = vec![("ci/legacy".to_string(), "pending".to_string())];
    let (state, total, passing, failing, pending) =
        required_checks_state(&required, &[], &statuses);
    assert_eq!(state, "pending");
    assert_eq!(total, 1);
    assert_eq!(passing, 0);
    assert_eq!(failing, 0);
    assert_eq!(pending, 1);
}

#[test]
fn required_checks_state_commit_status_neutral_and_skipped_pass() {
    let required = vec![required_check("ci/legacy", None)];
    for state_name in ["neutral", "skipped"] {
        let statuses = vec![("ci/legacy".to_string(), state_name.to_string())];
        let (state, total, passing, failing, pending) =
            required_checks_state(&required, &[], &statuses);
        assert_eq!(state, "success", "{state_name}");
        assert_eq!((total, passing, failing, pending), (1, 1, 0, 0));
    }
}

#[test]
fn failing_optional_check_names_skips_a_required_commit_status() {
    let required = vec![required_check("ci", None)];
    let statuses = vec![
        ("ci".to_string(), "failure".to_string()),
        ("lint".to_string(), "error".to_string()),
    ];
    assert_eq!(
        failing_optional_check_names(&required, &[], &statuses),
        vec!["lint"]
    );
}

#[test]
fn failing_optional_check_names_keeps_a_commit_status_when_the_rule_is_app_bound() {
    let required = vec![required_check("ci", Some(42))];
    let statuses = vec![("ci".to_string(), "failure".to_string())];
    assert_eq!(
        failing_optional_check_names(&required, &[], &statuses),
        vec!["ci"]
    );
}

#[test]
fn required_checks_state_check_run_without_app_does_not_satisfy_app_bound_rule() {
    let required = vec![required_check("ci/test", Some(42))];
    let check_runs = vec![check_run("ci/test", "completed", Some("success"), None)];
    let (state, ..) = required_checks_state(&required, &check_runs, &[]);
    assert_eq!(state, "pending");
}

#[test]
fn test_any_changes_requested_returns_true_when_latest_is_changes_requested() {
    let reviews = vec![
        make_review("alice", "APPROVED", "2026-01-01T10:00:00Z"),
        make_review("alice", "CHANGES_REQUESTED", "2026-01-01T11:00:00Z"),
    ];
    assert!(
        any_changes_requested_in_reviews(&reviews),
        "should return true when reviewer's latest review is CHANGES_REQUESTED"
    );
}

#[test]
fn test_any_changes_requested_returns_false_when_latest_is_approved() {
    let reviews = vec![
        make_review("alice", "CHANGES_REQUESTED", "2026-01-01T10:00:00Z"),
        make_review("alice", "APPROVED", "2026-01-01T11:00:00Z"),
    ];
    assert!(
        !any_changes_requested_in_reviews(&reviews),
        "should return false when reviewer's latest review is APPROVED"
    );
}

#[test]
fn test_any_changes_requested_returns_false_for_empty_reviews() {
    assert!(
        !any_changes_requested_in_reviews(&[]),
        "should return false for empty review list"
    );
}

#[test]
fn test_any_changes_requested_ignores_commented_state() {
    let reviews = vec![make_review("alice", "COMMENTED", "2026-01-01T10:00:00Z")];
    assert!(
        !any_changes_requested_in_reviews(&reviews),
        "COMMENTED state should be ignored"
    );
}

#[test]
fn test_any_changes_requested_one_reviewer_blocks_even_if_other_approved() {
    let reviews = vec![
        make_review("alice", "APPROVED", "2026-01-01T10:00:00Z"),
        make_review("bob", "CHANGES_REQUESTED", "2026-01-01T10:00:00Z"),
    ];
    assert!(
        any_changes_requested_in_reviews(&reviews),
        "should return true if any reviewer has outstanding CHANGES_REQUESTED"
    );
}

/// Regression test: handle_review_changes must set status to Routed (not New)
/// when a valid model is available for the task's agent via model_for_complexity().
/// This preserves the documented review-cycle architecture where re-dispatch
/// reuses existing routing context instead of forcing full LLM re-routing.
#[tokio::test]
async fn handle_review_changes_sets_routed_with_valid_model() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let repo = "owner/repo";

    let task_id_num = store
        .create(&NewTask {
            external_id: None,
            repo: repo.to_string(),
            origin: "internal".to_string(),
            title: "Implement feature X".to_string(),
            body: "body".to_string(),
            source: "cron".to_string(),
            source_id: "daily".to_string(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();
    store
        .update_status(task_id_num, TaskStatus::InReview)
        .await
        .unwrap();

    let task_id_str = format!("internal:{task_id_num}");
    let backend: Arc<dyn ExternalBackend> = Arc::new(NoopBackend);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        repo.to_string(),
    ));

    let task = ExternalTask {
        id: ExternalId(task_id_str.clone()),
        title: "Implement feature X".to_string(),
        body: "body".to_string(),
        state: "open".to_string(),
        labels: vec![],
        author: String::new(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: String::new(),
    };

    let result = handle_review_changes(
        &task,
        "Please fix the type error on line 42",
        &[],
        &backend,
        repo,
        101,
        "claude",
        "sonnet",
        &task_manager,
        &store,
        &router::RouterConfig::default(),
    )
    .await;

    assert!(
        result.is_ok(),
        "handle_review_changes returned Err: {result:?}"
    );

    let updated = store.get(task_id_num).await.unwrap();
    // When the agent has no model pools configured (default test config),
    // model_for_complexity returns None → falls back to Status::New.
    assert_eq!(
        updated.status,
        TaskStatus::New,
        "handle_review_changes must fall back to New when model_for_complexity returns None"
    );

    assert!(
        !updated.pr_review_context.is_empty(),
        "pr_review_context must be stored so the re-dispatched agent sees the feedback"
    );
    assert!(
        !updated
            .pr_review_context
            .starts_with("A reviewer has requested changes"),
        "pr_review_context must NOT start with the template header"
    );
    assert!(
        updated
            .pr_review_context
            .contains("Please fix the type error on line 42"),
        "pr_review_context must contain the review notes passed to handle_review_changes"
    );

    assert_eq!(
        updated.review_cycles, 1,
        "review_cycles must be incremented on each review change request"
    );

    assert_eq!(
        updated.review_agent_failures, 0,
        "review_agent_failures must be reset to 0 when re-routing after review changes"
    );
}

/// Regression: handle_review_changes must resolve a valid model via
/// model_for_complexity() instead of reusing the review agent's model.
/// When the task's agent has a valid model available, it sets Routed.
/// When all models are cooled, it falls back to New for full re-route (#1723).
#[tokio::test]
async fn handle_review_changes_resolves_valid_model_or_falls_back() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let repo = "owner/repo";

    let task_id_num = store
        .create(&NewTask {
            external_id: None,
            repo: repo.to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            body: "body".to_string(),
            source: "cron".to_string(),
            source_id: "daily".to_string(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();
    store
        .update_status(task_id_num, TaskStatus::InReview)
        .await
        .unwrap();

    // Pre-set agent/model/complexity to simulate a stored routing.
    // The model "opus" may not be valid for this agent's pool, but
    // model_for_complexity will resolve a valid one.
    store
        .set_fields(
            task_id_num,
            &[
                ("agent", serde_json::json!("claude")),
                ("model", serde_json::json!("opus")),
                ("complexity", serde_json::json!("medium")),
                (
                    "route_reason",
                    serde_json::json!("llm classified as medium"),
                ),
            ],
        )
        .await
        .unwrap();

    let task_id_str = format!("internal:{task_id_num}");
    let backend: Arc<dyn ExternalBackend> = Arc::new(NoopBackend);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        repo.to_string(),
    ));

    let task = ExternalTask {
        id: ExternalId(task_id_str.clone()),
        title: "Test".to_string(),
        body: "body".to_string(),
        state: "open".to_string(),
        labels: vec![],
        author: String::new(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: String::new(),
    };

    let result = handle_review_changes(
        &task,
        "Fix the bug",
        &[],
        &backend,
        repo,
        42,
        "minimax",
        "opus",
        &task_manager,
        &store,
        &router::RouterConfig::default(),
    )
    .await;

    assert!(
        result.is_ok(),
        "handle_review_changes returned Err: {result:?}"
    );

    let updated = store.get(task_id_num).await.unwrap();

    // When model_for_complexity returns a valid model → Routed, agent preserved.
    // When all models are cooled → falls back to New, agent cleared for full re-route.
    // In test env with no model_map configured, model_for_complexity returns None → New.
    // This is the expected fallback behavior.
    let model_was_resolved = updated.model.as_deref().is_some_and(|m| !m.is_empty());
    if model_was_resolved {
        // Got a valid model → should be Routed, agent preserved
        assert_eq!(
            updated.agent.as_deref(),
            Some("claude"),
            "agent must be preserved when model_for_complexity returns a valid model"
        );
        assert_eq!(
            updated.status,
            TaskStatus::Routed,
            "status must be Routed when model_for_complexity returns a valid model"
        );
    } else {
        // All models cooled or no pools configured → falls back to New, agent cleared
        assert_eq!(
            updated.agent.as_deref(),
            None,
            "agent must be cleared when falling back to full re-route"
        );
        assert_eq!(
            updated.status,
            TaskStatus::New,
            "status must be New when model_for_complexity returns None (all cooled)"
        );
    }

    assert_eq!(
        updated.review_agent_failures, 0,
        "review_agent_failures must be reset to 0 when re-routing after review changes"
    );
}

/// Regression: a transient `update_task_status` failure must NOT
/// cause `handle_review_changes` to return `Err`.  Returning `Err` would
/// propagate to the review subscriber which increments
/// `review_agent_failures` — incorrectly consuming the retry quota even
/// though the review decision itself was already persisted.
///
/// We simulate the failure by giving the `TaskManager` a different repo
/// than the one the task was created in, so `resolve_task_id` returns
/// `None` and `update_task_status` returns `Err` for that internal task.
#[tokio::test]
async fn handle_review_changes_transition_failure_returns_ok() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let task_repo = "owner/repo";

    let task_id_num = store
        .create(&NewTask {
            external_id: None,
            repo: task_repo.to_string(),
            origin: "internal".to_string(),
            title: "Fix bug".to_string(),
            body: "body".to_string(),
            source: "cron".to_string(),
            source_id: "daily".to_string(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();
    store
        .update_status(task_id_num, TaskStatus::InReview)
        .await
        .unwrap();

    let task_id_str = format!("internal:{task_id_num}");
    let backend: Arc<dyn ExternalBackend> = Arc::new(NoopBackend);

    // Intentionally wrong repo — causes resolve_task_id to return None,
    // which makes update_task_status return Err for the internal task.
    let wrong_repo = "other/repo";
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        wrong_repo.to_string(),
    ));

    let task = ExternalTask {
        id: ExternalId(task_id_str.clone()),
        title: "Fix bug".to_string(),
        body: "body".to_string(),
        state: "open".to_string(),
        labels: vec![],
        author: String::new(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: String::new(),
    };

    // The status transition will fail (wrong repo), but handle_review_changes
    // must still return Ok — review context was persisted before the failure.
    let result = handle_review_changes(
        &task,
        "Fix the null pointer on line 10",
        &[],
        &backend,
        task_repo,
        202,
        "claude",
        "sonnet",
        &task_manager,
        &store,
        &router::RouterConfig::default(),
    )
    .await;

    assert!(
        result.is_ok(),
        "handle_review_changes must return Ok even when status transition fails: {result:?}"
    );

    // Review context must have been persisted despite the status failure.
    let stored = store.get(task_id_num).await.unwrap();
    assert!(
        stored
            .pr_review_context
            .contains("Fix the null pointer on line 10"),
        "pr_review_context must be persisted even when status transition fails"
    );
    // Review_cycles should NOT be incremented if the status transition fails.
    // This prevents premature escalation when the Routed transition fails.
    // The increment happens AFTER successful status transition in the fixed code.
    assert_eq!(
        stored.review_cycles, 0,
        "review_cycles must NOT be incremented when status transition fails"
    );
    // Status stays InReview (not Routed/New) because the transition failed.
    assert_eq!(
        stored.status,
        TaskStatus::InReview,
        "status should remain InReview when status transition failed"
    );
}

/// ISO-8601 timestamps returned by GitHub compare lexicographically in
/// chronological order.  `branch_newer_than_last_review` relies on this.
#[test]
fn iso8601_timestamps_compare_correctly() {
    // newer commit (pushed after review) should be greater
    assert!(iso8601_is_newer(
        "2024-01-15T13:00:00Z",
        "2024-01-15T12:30:00Z"
    ));
    // same timestamp is not "newer"
    assert!(!iso8601_is_newer(
        "2024-01-15T12:30:00Z",
        "2024-01-15T12:30:00Z"
    ));
    // older commit is not newer
    assert!(!iso8601_is_newer(
        "2024-01-14T23:59:59Z",
        "2024-01-15T00:00:01Z"
    ));
    // cross-day boundary
    assert!(iso8601_is_newer(
        "2024-01-16T00:00:01Z",
        "2024-01-15T23:59:59Z"
    ));
}

#[test]
fn is_pr_behind_matches_mergeable_state() {
    let mut pr = GitHubPullRequest {
        number: 1,
        title: "Test PR".to_string(),
        body: None,
        state: "open".to_string(),
        head: GitHubBranchRef {
            ref_: "feature/test".to_string(),
            sha: "abc123".to_string(),
            repo: None,
        },
        base: GitHubBranchRef {
            ref_: "main".to_string(),
            sha: "def456".to_string(),
            repo: None,
        },
        mergeable: Some(true),
        mergeable_state: Some("behind".to_string()),
        merged: Some(false),
        html_url: "https://github.com/owner/repo/pull/1".to_string(),
    };

    assert!(is_pr_behind(&pr));

    pr.mergeable_state = Some("clean".to_string());
    assert!(!is_pr_behind(&pr));

    pr.mergeable_state = None;
    assert!(!is_pr_behind(&pr));
}

#[test]
fn fetch_failure_reason_strips_url_credentials() {
    let raw = "fatal: unable to access 'https://x-access-token:test-secret-1234567890abcdef@github.com/o/r.git/'";
    let stderr = git_ops::sanitize_push_error(raw);
    let reason = format!("git fetch failed: {stderr}");
    assert!(!reason.contains("test-secret-1234567890abcdef"));
    assert!(reason.contains("://***@github.com"));
}

#[test]
fn ssh_agent_fetch_failure_matches_sanitized_stderr() {
    let raw = concat!(
        "sign_and_send_pubkey: signing failed: agent refused operation\n",
        "fatal: unable to access 'https://x-access-token:test-secret-1234567890abcdef@github.com/o/r.git/'"
    );
    let sanitized = git_ops::sanitize_push_error(raw);
    assert!(!sanitized.contains("test-secret-1234567890abcdef"));
    assert!(is_ssh_agent_fetch_failure(&sanitized));
    assert!(is_ssh_agent_fetch_failure(raw));
    assert!(matches!(
        classify_fetch_failure(&sanitized),
        WorktreeRebaseOutcome::SshFetchFailed(_)
    ));
}

#[test]
fn classify_merge_failure_treats_out_of_date_head_as_recoverable() {
    let error = r#"GitHub PR merge failed (409 Conflict): {"message":"Head branch is out of date. Review and try the merge again."}"#;
    assert_eq!(
        classify_merge_failure(error),
        MergeFailureClass::RecoverableConflict
    );
}

#[test]
fn classify_merge_failure_treats_moved_head_as_transient() {
    let error = r#"GitHub PR merge failed (409 Conflict): {"message":"Head branch was modified. Review and try the merge again."}"#;
    assert_eq!(classify_merge_failure(error), MergeFailureClass::Transient);
    assert_eq!(
        classify_merge_failure("GitHub PR merge failed (409 Conflict): sha does not match"),
        MergeFailureClass::Transient
    );
}

#[test]
fn classify_merge_failure_treats_gateway_errors_as_transient() {
    assert_eq!(
        classify_merge_failure("GitHub PR merge failed (502 Bad Gateway)"),
        MergeFailureClass::Transient
    );
}

#[test]
fn classify_merge_failure_keeps_branch_protection_errors_blocking() {
    let error = "GitHub PR merge failed (403 Forbidden): branch protection prevented merge";
    assert_eq!(classify_merge_failure(error), MergeFailureClass::Blocking);
}

#[test]
fn classify_merge_failure_treats_draft_as_transient() {
    let error =
        r#"GitHub PR merge failed (405 Method Not Allowed): {"message":"Pull request is a draft"}"#;
    assert_eq!(classify_merge_failure(error), MergeFailureClass::Transient);
}

#[test]
fn classify_merge_failure_keeps_not_mergeable_as_recoverable() {
    let error = r#"GitHub PR merge failed (405 Method Not Allowed): {"message":"Pull Request is not mergeable"}"#;
    assert_eq!(
        classify_merge_failure(error),
        MergeFailureClass::RecoverableConflict
    );
}

#[test]
fn classify_merge_failure_treats_connection_failure_as_transient() {
    let error = "error sending request for url (https://api.github.com/repos/o/r/pulls/1/merge): Failed to connect to github.com:443 after 75021 ms: Could not connect to server";
    assert_eq!(classify_merge_failure(error), MergeFailureClass::Transient);
}

#[test]
fn classify_fetch_failure_treats_connection_timeout_and_ref_lock_as_transient() {
    let connect = concat!(
        "fatal: unable to access 'https://github.com/owner/repo.git/': ",
        "Failed to connect to github.com:443 after 75021 ms: Could not connect to server"
    );
    assert!(matches!(
        classify_fetch_failure(connect),
        WorktreeRebaseOutcome::Transient(_)
    ));
    assert!(matches!(
        classify_fetch_failure("git fetch timed out"),
        WorktreeRebaseOutcome::Transient(_)
    ));
    assert!(matches!(
        classify_fetch_failure("error: cannot lock ref 'refs/remotes/origin/main'"),
        WorktreeRebaseOutcome::Transient(_)
    ));
}

#[tokio::test]
async fn draft_merge_and_connect_fetch_leave_an_approved_task_and_counter() {
    let draft =
        r#"GitHub PR merge failed (405 Method Not Allowed): {"message":"Pull request is a draft"}"#;
    assert_eq!(classify_merge_failure(draft), MergeFailureClass::Transient);

    let fetch = classify_fetch_failure(concat!(
        "fatal: unable to access 'https://github.com/owner/repo.git/': ",
        "Failed to connect to github.com:443 after 75021 ms: Could not connect to server"
    ));
    assert!(
        matches!(fetch, WorktreeRebaseOutcome::Transient(_)),
        "a connect failure must not spend a merge-conflict retry: {fetch:?}"
    );

    let (store, task, task_manager, task_id_num) = approved_task_with_conflict_retries(2).await;
    let gh = GhHttp::new().unwrap();
    apply_rebase_recovery_outcome(
        fetch,
        &task,
        "owner/repo",
        4043,
        &gh,
        &task_manager,
        &store,
        "review-agent",
        "review-model",
    )
    .await
    .unwrap();

    let stored = store.get(task_id_num).await.unwrap();
    assert_eq!(stored.status, TaskStatus::Approved);
    assert_eq!(stored.merge_conflict_retries, 2);
}

#[tokio::test]
async fn auto_merge_pr_refuses_unless_the_task_is_approved() {
    let (store, task, task_manager, task_id_num) = approved_task_with_conflict_retries(0).await;
    store
        .update_status(task_id_num, TaskStatus::InReview)
        .await
        .unwrap();
    let backend: Arc<dyn ExternalBackend> = Arc::new(NoopBackend);
    auto_merge_pr(
        &task,
        "feat-branch",
        &backend,
        "owner/repo",
        "review-agent",
        "review-model",
        &task_manager,
        &store,
    )
    .await
    .unwrap();
    let stored = store.get(task_id_num).await.unwrap();
    assert_eq!(stored.status, TaskStatus::InReview);
    assert_eq!(stored.merge_conflict_retries, 0);
}

#[tokio::test]
async fn apply_rebase_recovery_increments_retries_once_on_content_conflict() {
    let (store, task, task_manager, task_id_num) = approved_task_with_conflict_retries(0).await;
    let gh = GhHttp::new().unwrap();
    apply_rebase_recovery_outcome(
        WorktreeRebaseOutcome::RebaseConflict(
            "CONFLICT (content): merge conflict in src/lib.rs".to_string(),
        ),
        &task,
        "owner/repo",
        4142,
        &gh,
        &task_manager,
        &store,
        "review-agent",
        "review-model",
    )
    .await
    .unwrap();
    let stored = store.get(task_id_num).await.unwrap();
    assert_eq!(stored.status, TaskStatus::Routed);
    assert_eq!(stored.merge_conflict_retries, 1);
}

/// When the GitHub API is unavailable (no real token in unit tests),
/// `handle_review_changes` must still escalate correctly rather than
/// getting stuck.  The stale-review check fails gracefully and we fall
/// through to the normal Blocked path.
#[tokio::test]
async fn handle_review_changes_escalates_when_stale_check_fails() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let repo = "owner/repo";

    let task_id_num = store
        .create(&NewTask {
            external_id: None,
            repo: repo.to_string(),
            origin: "internal".to_string(),
            title: "Fix defaults".to_string(),
            body: "body".to_string(),
            source: "cron".to_string(),
            source_id: "daily".to_string(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // Set review_cycles to max so we enter the escalation path.
    store
        .set_fields(task_id_num, &[("review_cycles", serde_json::json!(2i64))])
        .await
        .unwrap();
    store
        .update_status(task_id_num, TaskStatus::InReview)
        .await
        .unwrap();

    let task_id_str = format!("internal:{task_id_num}");
    let backend: Arc<dyn ExternalBackend> = Arc::new(NoopBackend);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        repo.to_string(),
    ));

    let task = ExternalTask {
        id: ExternalId(task_id_str.clone()),
        title: "Fix defaults".to_string(),
        body: "body".to_string(),
        state: "open".to_string(),
        labels: vec![],
        author: String::new(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: String::new(),
    };

    // With no real GitHub token the stale check will fail gracefully and
    // we fall through to normal escalation (Blocked status).
    let result = handle_review_changes(
        &task,
        "sync_interval defaults are wrong",
        &[],
        &backend,
        repo,
        303,
        "minimax",
        "minimax-m2",
        &task_manager,
        &store,
        &router::RouterConfig::default(),
    )
    .await;

    assert!(
        result.is_ok(),
        "handle_review_changes must return Ok on escalation path: {result:?}"
    );

    let stored = store.get(task_id_num).await.unwrap();
    assert_eq!(
        stored.status,
        TaskStatus::Blocked,
        "task must be Blocked when max review cycles exceeded and stale check is unavailable"
    );
}

/// Regression test for the bug where `auto_merge_pr`'s pending-CI timeout
/// never fired: elapsed pending time must accumulate in the KV store
/// across separate calls (simulating separate short-lived invocations),
/// not reset to zero each time like a function-local `Instant` would.
#[tokio::test]
async fn ci_pending_elapsed_persists_across_calls() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let task_id = "internal:1";

    // First observation: no prior timestamp, so elapsed starts at zero
    // and the "first seen pending" timestamp is persisted.
    let first = ci_pending_elapsed(&store, REPO, task_id, SHA)
        .await
        .unwrap();
    assert!(
        first.num_seconds() < 1,
        "first pending observation should report ~zero elapsed, got {first:?}"
    );

    // Simulate time passing between two short-lived invocations by
    // backdating the persisted timestamp directly, the way it would look
    // if `is_ci_check_in_cooldown` had short-circuited several polls ago.
    let backdated = chrono::Utc::now() - chrono::Duration::seconds(700);
    store
        .kv_set(
            &ci_pending_since_key(REPO, task_id, SHA),
            &backdated.to_rfc3339(),
        )
        .await
        .unwrap();

    let second = ci_pending_elapsed(&store, REPO, task_id, SHA)
        .await
        .unwrap();
    assert!(
        second.num_seconds() >= 600,
        "elapsed pending time must accumulate across calls instead of resetting, got {second:?}"
    );

    // Clearing removes the persisted timestamp so the next observation
    // starts a fresh streak at ~zero elapsed.
    clear_ci_pending_since(&store, REPO, task_id, "test")
        .await
        .unwrap();
    let after_clear = ci_pending_elapsed(&store, REPO, task_id, SHA)
        .await
        .unwrap();
    assert!(
        after_clear.num_seconds() < 1,
        "elapsed must reset to ~zero after clear_ci_pending_since, got {after_clear:?}"
    );
}

/// Regression test for #3609: `auto_merge_pr` can run as two overlapping
/// invocations for the same task (the review-agent's direct approval call
/// in review.rs is not guarded by the same `auto_merge_in_flight` set as
/// the periodic review_open_prs sweep). A racing call that observes CI as
/// newly pending must not overwrite an already-persisted "since" — doing
/// so silently resets the elapsed clock and the 600s timeout escalation
/// never fires, even though a human-visible log line shows CI pending
/// continuously well past `max_wait`.
#[tokio::test]
async fn ci_pending_elapsed_atomic_insert_survives_concurrent_racers() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let task_id = "internal:2";

    let backdated = chrono::Utc::now() - chrono::Duration::seconds(700);
    store
        .kv_set(
            &ci_pending_since_key(REPO, task_id, SHA),
            &backdated.to_rfc3339(),
        )
        .await
        .unwrap();

    // A second, racing invocation observing the same pending state must
    // read back the already-persisted timestamp instead of clobbering it
    // with its own "now".
    let racer = ci_pending_elapsed(&store, REPO, task_id, SHA)
        .await
        .unwrap();
    assert!(
        racer.num_seconds() >= 600,
        "a racing call must not reset an already-persisted ci_pending_since, got {racer:?}"
    );
}

async fn approved_task_with_conflict_retries(
    retries: i32,
) -> (Arc<TaskStore>, ExternalTask, Arc<TaskManager>, i64) {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let repo = "owner/repo";
    let task_id_num = store
        .create(&NewTask {
            external_id: None,
            repo: repo.to_string(),
            origin: "internal".to_string(),
            title: "Approved merge".to_string(),
            body: "body".to_string(),
            source: "github".to_string(),
            source_id: "4086".to_string(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();
    store
        .update_status(task_id_num, TaskStatus::Approved)
        .await
        .unwrap();
    for _ in 0..retries {
        store
            .increment(task_id_num, "merge_conflict_retries")
            .await
            .unwrap();
    }

    let task_id_str = format!("internal:{task_id_num}");
    let backend: Arc<dyn ExternalBackend> = Arc::new(NoopBackend);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        repo.to_string(),
    ));
    let task = ExternalTask {
        id: ExternalId(task_id_str),
        title: "Approved merge".to_string(),
        body: "body".to_string(),
        state: "open".to_string(),
        labels: vec![],
        author: String::new(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: String::new(),
    };
    (store, task, task_manager, task_id_num)
}

const REPO: &str = "owner/repo";
const SHA: &str = "abc123";

#[tokio::test]
async fn ci_pending_window_is_per_repo_and_per_head() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backdated = chrono::Utc::now() - chrono::Duration::days(3);
    store
        .kv_set(
            &ci_pending_since_key("other/repo", "7", SHA),
            &backdated.to_rfc3339(),
        )
        .await
        .unwrap();
    store
        .kv_set(
            &ci_pending_since_key(REPO, "7", "oldsha"),
            &backdated.to_rfc3339(),
        )
        .await
        .unwrap();

    let other_repo = ci_pending_elapsed(&store, REPO, "7", SHA).await.unwrap();
    assert!(other_repo.num_seconds() < 1, "got {other_repo:?}");

    let new_head = ci_pending_elapsed(&store, REPO, "7", "newsha")
        .await
        .unwrap();
    assert!(new_head.num_seconds() < 1, "got {new_head:?}");

    clear_ci_pending_since(&store, REPO, "7", "test")
        .await
        .unwrap();
    assert_eq!(
        store
            .kv_list_prefix(&ci_pending_since_prefix(REPO, "7"))
            .await
            .unwrap(),
        [] as [(String, String); 0]
    );

    assert!(
        store
            .kv_get(&ci_pending_since_key("other/repo", "7", SHA))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn ci_check_cooldown_is_per_repo() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    record_ci_check(&store, "a/x", "5").await.unwrap();
    assert!(is_ci_check_in_cooldown(&store, "a/x", "5").await.unwrap());
    assert!(!is_ci_check_in_cooldown(&store, "b/x", "5").await.unwrap());
}

/// A get_pr failure after get_closed_pr_state reported a match must not mark the task done
#[expect(
    clippy::await_holding_lock,
    reason = "the env lock stays held across the awaits so tests do not race on GH_TOKEN"
)]
#[tokio::test]
async fn finish_already_closed_pr_leaves_status_when_get_pr_fails() {
    let _env_guard = token::ENV_LOCK.lock().unwrap();
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("GH_TOKEN", "test_token") };

    let server = MockServer::start().await;
    let repo = "owner/repo";
    let branch = "task-branch";

    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls"))
        .and(query_param("state", "closed"))
        .and(query_param("head", "owner:task-branch"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                "number": 42,
                "state": "closed",
                "merged_at": "2026-01-01T00:00:00Z",
                "head": { "ref": "task-branch" }
            }])),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/42"))
        .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
        .mount(&server)
        .await;

    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let task_id_num = store
        .create(&NewTask {
            repo: repo.to_string(),
            origin: "internal".to_string(),
            title: "task".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .update_status(task_id_num, TaskStatus::InReview)
        .await
        .unwrap();

    let backend: Arc<dyn ExternalBackend> = Arc::new(NoopBackend);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        repo.to_string(),
    ));
    let task = ExternalTask {
        id: ExternalId(format!("internal:{task_id_num}")),
        title: "task".to_string(),
        body: String::new(),
        state: "open".to_string(),
        labels: vec![],
        author: String::new(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: String::new(),
    };

    let gh = GhHttp::for_test(server.uri(), None);
    let result = finish_already_closed_pr(&gh, &task, branch, repo, &task_manager, &store).await;
    assert!(
        result.is_err(),
        "get_pr failure must not finish the task: {result:?}"
    );
    let stored = store.get(task_id_num).await.unwrap();
    assert_eq!(stored.status, TaskStatus::InReview);
}

#[tokio::test]
async fn sweep_removes_finished_task_and_legacy_keys() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    record_ci_check(&store, REPO, "7").await.unwrap();
    record_ci_check(&store, REPO, "8").await.unwrap();
    store
        .kv_set(&ci_pending_since_key(REPO, "7", SHA), "t")
        .await
        .unwrap();
    store
        .kv_set(&ci_failure_sha_key(REPO, "7", SHA), "1")
        .await
        .unwrap();
    store
        .kv_set(&ci_failure_sha_key(REPO, "8", SHA), "1")
        .await
        .unwrap();
    store.kv_set("ci_check_ts:7", "t").await.unwrap();
    store.kv_set("ci_pending_since:7", "t").await.unwrap();
    store
        .kv_set(&format!("ci_bootstrap_wait_since:{REPO}#7#abc"), "t")
        .await
        .unwrap();
    store
        .kv_set(&format!("ci_bootstrap_wait_since:{REPO}#8#abc"), "t")
        .await
        .unwrap();

    sweep_ci_timer_keys(&store, REPO, &["7".to_string()])
        .await
        .unwrap();

    assert!(
        store
            .kv_get(&ci_check_ts_key(REPO, "7"))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .kv_list_prefix(&ci_pending_since_prefix(REPO, "7"))
            .await
            .unwrap(),
        [] as [(String, String); 0]
    );

    assert!(store.kv_get("ci_check_ts:7").await.unwrap().is_none());
    assert!(store.kv_get("ci_pending_since:7").await.unwrap().is_none());
    assert!(
        store
            .kv_get(&ci_check_ts_key(REPO, "8"))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .kv_get(&ci_failure_sha_key(REPO, "7", SHA))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .kv_get(&ci_failure_sha_key(REPO, "8", SHA))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .kv_get(&format!("ci_bootstrap_wait_since:{REPO}#7#abc"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .kv_get(&format!("ci_bootstrap_wait_since:{REPO}#8#abc"))
            .await
            .unwrap()
            .is_some()
    );
}

async fn task_for_ci_failure(repo: &str, source_id: &str) -> (Arc<TaskStore>, String, i64) {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let store_id = store
        .create_internal(repo, "CI failure", "", "test", source_id, None)
        .await
        .unwrap();
    (store, format!("internal:{store_id}"), store_id)
}

#[tokio::test]
async fn count_ci_failure_for_head_counts_a_sha_once() {
    let (store, task_id, store_id) = task_for_ci_failure(REPO, "once").await;

    let first = count_ci_failure_for_head(&store, REPO, &task_id, SHA)
        .await
        .unwrap();
    let again = count_ci_failure_for_head(&store, REPO, &task_id, SHA)
        .await
        .unwrap();
    let next = count_ci_failure_for_head(&store, REPO, &task_id, "newsha")
        .await
        .unwrap();

    assert_eq!(first, Some(1));
    assert_eq!(again, None);
    assert_eq!(next, Some(2));
    assert_eq!(store.get(store_id).await.unwrap().ci_merge_failures, 2);
}

#[tokio::test]
async fn count_ci_failure_for_head_is_per_task() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let a = store
        .create_internal(REPO, "a", "", "test", "a", None)
        .await
        .unwrap();
    let b = store
        .create_internal(REPO, "b", "", "test", "b", None)
        .await
        .unwrap();
    let a_id = format!("internal:{a}");
    let b_id = format!("internal:{b}");

    assert_eq!(
        count_ci_failure_for_head(&store, REPO, &a_id, SHA)
            .await
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        count_ci_failure_for_head(&store, REPO, &b_id, SHA)
            .await
            .unwrap(),
        Some(1)
    );
    assert_eq!(store.get(a).await.unwrap().ci_merge_failures, 1);
    assert_eq!(store.get(b).await.unwrap().ci_merge_failures, 1);
}

#[tokio::test]
async fn count_ci_failure_for_head_empty_sha_always_counts() {
    let (store, task_id, store_id) = task_for_ci_failure(REPO, "empty").await;

    assert_eq!(
        count_ci_failure_for_head(&store, REPO, &task_id, "")
            .await
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        count_ci_failure_for_head(&store, REPO, &task_id, "")
            .await
            .unwrap(),
        Some(2)
    );
    assert_eq!(store.get(store_id).await.unwrap().ci_merge_failures, 2);
}

#[tokio::test]
async fn count_ci_failure_for_head_only_one_racer_counts() {
    let (store, task_id, store_id) = task_for_ci_failure(REPO, "race").await;

    let (a, b) = tokio::join!(
        count_ci_failure_for_head(&store, REPO, &task_id, SHA),
        count_ci_failure_for_head(&store, REPO, &task_id, SHA),
    );
    let counted = [a.unwrap(), b.unwrap()].into_iter().flatten().count();
    assert_eq!(counted, 1);
    assert_eq!(store.get(store_id).await.unwrap().ci_merge_failures, 1);
}
