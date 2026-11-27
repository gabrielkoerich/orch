use super::parse::review_decision_from_response;
use super::pr::count_ahead_commits;
use super::publish::build_pr_review_comment;
use super::publish::publish_review;
use super::publish::review_app_for;
use super::publish::review_status;
use crate::github::app;
use crate::github::app::AppAuth;
use std::env;
use std::process;

use super::*;
use crate::backends::ExternalId;
use crate::backends::ExternalTask;
use crate::backends::test_helpers::NoopBackend;
use crate::engine::router::RouterConfig;
use crate::engine::runner::response::ReviewIssue;
use crate::github::token;
use crate::github::types::{GitHubReview, GitHubReviewComment, GitHubUser, PullRequestReview};
use crate::review_mode::ReviewMode;
use crate::store::{TaskStatus, TaskStore};
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_partial_json;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::query_param;

#[test]
fn a_review_decision_counts_only_from_a_successful_result() {
    let approve = r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"decision\":\"approve\",\"notes\":\"LGTM\",\"issues\":[]}"}"#;
    let prose = r#"{"type":"result","subtype":"success","is_error":false,"result":"I have finished looking at the code."}"#;
    let failed = r#"{"type":"result","subtype":"error","is_error":true,"result":"{\"decision\":\"approve\",\"notes\":\"\",\"issues\":[]}"}"#;
    let thinking =
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Reading the diff"}]}}"#;

    assert!(output_has_review_decision("claude", approve));
    assert!(!output_has_review_decision("claude", prose));
    assert!(!output_has_review_decision("claude", failed));
    assert!(!output_has_review_decision("claude", thinking));
    assert!(!output_has_review_decision("claude", ""));
}

#[test]
fn a_review_gets_15_minutes_unless_configured_and_never_under_5() {
    assert_eq!(review_timeout_secs(None), 900);
    assert_eq!(review_timeout_secs(Some(600)), 600);
    assert_eq!(review_timeout_secs(Some(60)), 300);
    assert_eq!(review_timeout_secs(Some(3600)), 3600);
}

// ── outcome_for_agent_error ─────────────────────────────────────────────

// ── no_session_review_floor ─────────────────────────────────────────────

#[test]
fn no_session_review_floor_raises_a_limit_under_the_review_timeout() {
    assert_eq!(
        no_session_review_floor(600, 900),
        900 + REVIEW_RESULT_DELIVERY_SECS
    );
}

#[test]
fn no_session_review_floor_keeps_a_limit_over_the_review_timeout() {
    assert_eq!(no_session_review_floor(1800, 900), 1800);
}

#[test]
fn no_session_review_floor_clears_the_review_timeout_by_the_delivery_margin() {
    for (configured, review_timeout) in [(0, 0), (1800, 1800), (120, 7200)] {
        let floored = no_session_review_floor(configured, review_timeout);
        assert!(
            floored >= review_timeout + REVIEW_RESULT_DELIVERY_SECS,
            "{floored} must leave a finished review time to deliver its result"
        );
    }
}

#[test]
fn outcome_for_agent_error_kimi_billing_cycle_is_not_rate_limit() {
    // Regression test for issue #3529: a review run hitting Kimi's billing-cycle
    // exhaustion (task 156734 in the bug report) must not be stored as a plain
    // "rate_limit" outcome.
    let err = runner::agents::AgentError::RateLimit {
        message: "Failed to authenticate. API Error: 403 You've reached your usage limit \
                      for this billing cycle. Your quota will be refreshed soon."
            .to_string(),
    };
    assert_eq!(outcome_for_agent_error(&err), "billing_cycle_exhausted");
}

#[test]
fn outcome_for_agent_error_generic_rate_limit_unaffected() {
    let err = runner::agents::AgentError::RateLimit {
        message: "429 Too Many Requests".to_string(),
    };
    assert_eq!(outcome_for_agent_error(&err), "rate_limit");
}

#[test]
fn outcome_for_agent_error_generic_auth_unaffected() {
    let err = runner::agents::AgentError::Auth {
        message: "401 Unauthorized".to_string(),
    };
    assert_eq!(outcome_for_agent_error(&err), "auth_error");
}

// ── parse_pr_number_from_url ────────────────────────────────────────────

#[test]
fn parse_pr_number_standard_url() {
    assert_eq!(
        parse_pr_number_from_url("https://github.com/owner/repo/pull/42"),
        Ok(42)
    );
}

#[test]
fn parse_pr_number_trailing_slash() {
    assert_eq!(
        parse_pr_number_from_url("https://github.com/owner/repo/pull/42/"),
        Ok(42)
    );
}

#[test]
fn parse_pr_number_with_query_string() {
    assert_eq!(
        parse_pr_number_from_url("https://github.com/owner/repo/pull/42?tab=files"),
        Ok(42)
    );
}

#[test]
fn parse_pr_number_with_hash_fragment() {
    assert_eq!(
        parse_pr_number_from_url("https://github.com/owner/repo/pull/42#issuecomment-123"),
        Ok(42)
    );
}

#[test]
fn parse_pr_number_with_query_and_hash() {
    assert_eq!(
        parse_pr_number_from_url("https://github.com/owner/repo/pull/42?quick_pull=1#top"),
        Ok(42)
    );
}

#[test]
fn parse_pr_number_with_leading_whitespace() {
    assert_eq!(
        parse_pr_number_from_url("  https://github.com/owner/repo/pull/42\n"),
        Ok(42)
    );
}

#[test]
fn parse_pr_number_wrong_domain() {
    let url = "https://gitlab.com/owner/repo/merge_requests/42";
    let result = parse_pr_number_from_url(url);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .contains("does not start with 'https://github.com/'")
    );
}

#[test]
fn parse_pr_number_missing_pull_segment() {
    // Issue URL — not a PR URL
    let url = "https://github.com/owner/repo/issues/42";
    let result = parse_pr_number_from_url(url);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("missing '/pull/' segment"));
}

#[test]
fn parse_pr_number_zero_is_rejected() {
    let url = "https://github.com/owner/repo/pull/0";
    let result = parse_pr_number_from_url(url);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("cannot be zero"));
}

#[test]
fn parse_pr_number_non_numeric_segment() {
    let url = "https://github.com/owner/repo/pull/abc";
    let result = parse_pr_number_from_url(url);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not a valid positive integer"));
}

#[test]
fn parse_pr_number_empty_string() {
    let result = parse_pr_number_from_url("");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("empty"));
}

#[test]
fn parse_pr_number_http_not_https() {
    // HTTP URLs are not accepted — only HTTPS
    let url = "http://github.com/owner/repo/pull/42";
    let result = parse_pr_number_from_url(url);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .contains("does not start with 'https://github.com/'")
    );
}

// ── format_review_error ─────────────────────────────────────────────────
//
// Regression coverage for issue #3149: review runs were persisting raw
// `api_retry` NDJSON fragments into `task_runs.error` because the review
// pipeline formatted rate-limit errors with `err.to_string()` instead of
// routing the raw provider message through `summarize_rate_limit_error`.

#[test]
fn format_review_error_summarises_rate_limit_fragment_with_api_retry() {
    // Real-world fragment captured from a minimax review run (issue #3149).
    // The provider message contains a complete api_retry JSON line so the
    // summariser should produce a clean `status=… after … attempts` summary.
    let raw_fragment = "{\"attempt\":6,\"max_retries\":10,\"retry_delay_ms\":17266.789,\"error_status\":429,\"error\":\"rate_limit\",\"session_id\":\"5c7148ee\"}\n\
                            {\"type\":\"system\",\"subtype\":\"api_retry\",\"attempt\":7,\"max_retries\":10,\"retry_delay_ms\":35162.66,\"error_status\":429,\"error\":\"rate_limit\",\"session_id\":\"5c7148ee\"}";
    let err = runner::agents::AgentError::RateLimit {
        message: raw_fragment.to_string(),
    };

    let stored = format_review_error("minimax", &err);

    // Must not contain raw JSON braces or the api_retry marker — these are
    // exactly the leaks that filled `task_runs.error` with noisy payloads.
    assert!(
        !stored.contains("{\"type\""),
        "stored error must not contain raw NDJSON envelope, got: {stored}"
    );
    assert!(
        !stored.contains("subtype"),
        "stored error must not contain raw api_retry fragment, got: {stored}"
    );
    assert!(
        !stored.contains("session_id"),
        "stored error must not contain session_id from provider payload, got: {stored}"
    );
    assert_eq!(
        stored,
        "minimax rate limit: status=429 after 7 attempts (last delay 35s)"
    );
}

#[test]
fn format_review_error_summarises_truncated_fragment_without_subtype_marker() {
    // Mid-object fragment that begins after the `subtype` key was cut off — this
    // is the shape that landed in `task_runs.error` for rows 13634/13542/9821
    // before the fix. The summariser's fallback path should emit a compact
    // hint rather than echoing the raw payload.
    let raw_fragment = ",\"attempt\":5,\"max_retries\":10,\"retry_delay_ms\":9034.81,\"error_status\":429,\"error\":\"rate_limit\",\"session_id\":\"992e17d3\"";
    let err = runner::agents::AgentError::RateLimit {
        message: raw_fragment.to_string(),
    };

    let stored = format_review_error("glm", &err);

    assert!(stored.starts_with("glm rate limit:"));
    assert!(
        !stored.contains("session_id"),
        "stored error must not leak session_id from truncated fragment, got: {stored}"
    );
    // The fragment first-line truncation path is bounded — verify the stored
    // string stays well under the 512-char cap suggested by the bug report.
    assert!(
        stored.len() <= 200,
        "stored error should be compact (<=200 chars), got {} chars: {stored}",
        stored.len()
    );
}

#[test]
fn format_review_error_preserves_non_rate_limit_variants() {
    let err = runner::agents::AgentError::Auth {
        message: "invalid bearer token".to_string(),
    };
    assert_eq!(
        format_review_error("kimi", &err),
        "kimi auth error: invalid bearer token"
    );

    let err = runner::agents::AgentError::Timeout {
        elapsed: Duration::from_secs(1801),
    };
    assert_eq!(
        format_review_error("kimi", &err),
        "kimi timed out after 1801s"
    );
}

pub(super) fn git(dir: &Path, args: &[&str]) {
    let mut cmd = process::Command::new("git");
    cmd.current_dir(dir);
    // Global commit.gpgsign calls the operator's signer (1Password)
    if args.first() == Some(&"commit") {
        cmd.args(["-c", "commit.gpgsign=false"]);
    }
    let status = cmd.args(args).status().unwrap();
    assert!(status.success(), "git {args:?} failed");
}

#[test]
fn test_pull_request_review_requests_changes() {
    let review = PullRequestReview {
        review: GitHubReview {
            id: 1,
            user: GitHubUser {
                login: "reviewer".to_string(),
            },
            body: Some("Please fix".to_string()),
            state: "CHANGES_REQUESTED".to_string(),
            html_url: None,
            submitted_at: "2024-01-01T00:00:00Z".to_string(),
            commit_id: None,
        },
        comments: vec![],
    };
    assert!(review.requests_changes());
}

#[test]
fn test_pull_request_review_does_not_request_changes() {
    let review = PullRequestReview {
        review: GitHubReview {
            id: 1,
            user: GitHubUser {
                login: "reviewer".to_string(),
            },
            body: Some("LGTM".to_string()),
            state: "APPROVED".to_string(),
            html_url: None,
            submitted_at: "2024-01-01T00:00:00Z".to_string(),
            commit_id: None,
        },
        comments: vec![],
    };
    assert!(!review.requests_changes());
}

#[test]
fn test_pull_request_review_actionable_comments_filters_empty_and_replies() {
    let review = PullRequestReview {
        review: GitHubReview {
            id: 1,
            user: GitHubUser {
                login: "reviewer".to_string(),
            },
            body: None,
            state: "CHANGES_REQUESTED".to_string(),
            html_url: None,
            submitted_at: "2024-01-01T00:00:00Z".to_string(),
            commit_id: None,
        },
        comments: vec![
            GitHubReviewComment {
                id: 1,
                user: GitHubUser {
                    login: "reviewer".to_string(),
                },
                body: "Fix this issue".to_string(),
                path: "src/main.rs".to_string(),
                line: Some(10),
                original_line: Some(10),
                commit_id: "abc".to_string(),
                original_commit_id: "abc".to_string(),
                html_url: "url".to_string(),
                created_at: "2024-01-01T00:00:00Z".to_string(),
                updated_at: "2024-01-01T00:00:00Z".to_string(),
                in_reply_to_id: None,
                diff_hunk: Some(
                    "@@ -8,5 +8,5 @@ fn main() {\n-    let x = 1;\n+    let x = 2;".to_string(),
                ),
            },
            GitHubReviewComment {
                id: 2,
                user: GitHubUser {
                    login: "reviewer".to_string(),
                },
                body: String::new(),
                path: "src/lib.rs".to_string(),
                line: Some(20),
                original_line: Some(20),
                commit_id: "abc".to_string(),
                original_commit_id: "abc".to_string(),
                html_url: "url".to_string(),
                created_at: "2024-01-01T00:00:00Z".to_string(),
                updated_at: "2024-01-01T00:00:00Z".to_string(),
                in_reply_to_id: None,
                diff_hunk: None,
            },
            GitHubReviewComment {
                id: 3,
                user: GitHubUser {
                    login: "reviewer".to_string(),
                },
                body: "Reply to this".to_string(),
                path: "src/lib.rs".to_string(),
                line: Some(30),
                original_line: Some(30),
                commit_id: "abc".to_string(),
                original_commit_id: "abc".to_string(),
                html_url: "url".to_string(),
                created_at: "2024-01-01T00:00:00Z".to_string(),
                updated_at: "2024-01-01T00:00:00Z".to_string(),
                in_reply_to_id: Some(1),
                diff_hunk: None,
            },
        ],
    };
    let actionable = review.actionable_comments();
    assert_eq!(actionable.len(), 1);
    assert_eq!(actionable[0].id, 1);
    assert_eq!(actionable[0].body, "Fix this issue");
    assert_eq!(actionable[0].path, "src/main.rs");
    assert_eq!(
        actionable[0].diff_hunk.as_ref().unwrap(),
        "@@ -8,5 +8,5 @@ fn main() {\n-    let x = 1;\n+    let x = 2;"
    );
}

#[test]
fn test_router_config_model_for_complexity_returns_none_without_config() {
    // With no model_map configured, model_for_complexity must return None rather than
    // silently using a hardcoded model that may not exist for the given agent.
    let cfg = RouterConfig::default();
    assert!(cfg.model_for_complexity("claude", "simple", "").is_none());
    assert!(cfg.model_for_complexity("opencode", "review", "").is_none());
    assert!(
        cfg.model_for_complexity("unknown_agent_xyz", "simple", "")
            .is_none()
    );
}

#[test]
fn review_status_maps_decisions() {
    assert_eq!(
        review_status(&ReviewDecision::Approve { issues: vec![] })
            .unwrap()
            .0,
        "success"
    );
    let changes = ReviewDecision::RequestChanges {
        notes: String::new(),
        issues: vec![],
    };
    assert_eq!(review_status(&changes).unwrap().0, "failure");
    assert!(review_status(&ReviewDecision::Failed("x".into())).is_none());
}

fn sample_review_issue(severity: &str) -> runner::response::ReviewIssue {
    runner::response::ReviewIssue {
        file: "src/engine/tick.rs".into(),
        line: Some(142),
        severity: severity.into(),
        description: "index out of bounds on the last element".into(),
    }
}

fn sample_review_response(
    decision: &str,
    test_results: Option<&str>,
    issues: Vec<runner::response::ReviewIssue>,
) -> runner::response::ReviewResponse {
    runner::response::ReviewResponse {
        decision: decision.into(),
        notes: "looks fine".into(),
        test_results: test_results.map(str::to_string),
        issues,
    }
}

#[test]
fn approve_with_failing_tests_becomes_request_changes() {
    let decision =
        review_decision_from_response(sample_review_response("approve", Some("fail"), vec![]));
    match decision {
        ReviewDecision::RequestChanges { notes, issues } => {
            assert!(
                notes.contains("test_results: fail"),
                "notes must name the failing test run, got {notes:?}"
            );
            assert!(issues.is_empty(), "{issues:?}");
        }
        other => panic!("expected RequestChanges, got {other:?}"),
    }
}

#[test]
fn approve_with_error_issue_becomes_request_changes() {
    let issue = sample_review_issue("error");
    let decision = review_decision_from_response(sample_review_response(
        "approve",
        Some("pass"),
        vec![issue.clone()],
    ));
    match decision {
        ReviewDecision::RequestChanges { issues, .. } => {
            assert_eq!(issues, vec![issue]);
        }
        other => panic!("expected RequestChanges, got {other:?}"),
    }
}

#[test]
fn approve_with_warning_issue_stays_approve_and_comment_lists_it() {
    let issue = sample_review_issue("warning");
    let decision = review_decision_from_response(sample_review_response(
        "approve",
        Some("pass"),
        vec![issue.clone()],
    ));
    match &decision {
        ReviewDecision::Approve { issues } => {
            assert_eq!(issues, &vec![issue.clone()]);
        }
        other => panic!("expected Approve, got {other:?}"),
    }
    let comment = build_pr_review_comment(&decision, "looks fine");
    assert!(
        comment.contains("Approve"),
        "approve header missing from {comment}"
    );
    assert!(
        comment.contains(&issue.description),
        "warning issue missing from {comment}"
    );
    assert!(
        comment.contains("warning"),
        "warning severity missing from {comment}"
    );
}

#[test]
fn review_started_comment_includes_agent_and_model() {
    let expected_footer = attribution_footer("Review started", "kimi", Some("opus"));
    let expected = format!("🔍 Automated review started{expected_footer}");
    assert_eq!(review_started_comment("kimi", "opus"), expected);
}

// ── build_review_context: no worktree/branch/pr (issue #3623) ───────────

fn no_code_review_task(id: &str) -> ExternalTask {
    ExternalTask {
        id: ExternalId(id.to_string()),
        title: "opencode discovery timeout".to_string(),
        body: String::new(),
        state: "open".to_string(),
        labels: vec![],
        author: "tester".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: "https://example.com/issues/1".to_string(),
    }
}

#[test]
fn a_review_agent_cannot_change_the_repository_remotes() {
    let tools = build_review_disallowed_tools(Path::new("/tmp/wt"));
    for guard in runner::git_ops::remote_change_tools() {
        assert!(tools.contains(&guard), "missing {guard}");
    }
}

#[tokio::test]
async fn build_review_context_reroutes_external_task_with_no_code() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let repo = "owner/repo";
    let ext = no_code_review_task("3617");
    store.ensure_external_task(repo, &ext).await.unwrap();

    let backend = Arc::new(NoopBackend);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        repo.to_string(),
    ));
    let router = Arc::new(RwLock::new(Router::new(RouterConfig::default())));

    let phase = build_review_context(&ext, repo, &router, &task_manager, &store)
        .await
        .unwrap();

    assert!(
        matches!(phase, ReviewPhase::EarlyReturn(ReviewDecision::Rerouted)),
        "external task with no worktree/branch/pr must reroute, not skip-to-done"
    );

    let stored = store
        .get_by_external_id(repo, &ext.id.0)
        .await
        .unwrap()
        .expect("task must still exist");
    assert_eq!(stored.status, TaskStatus::New);
    assert!(stored.agent.is_none());
    assert!(stored.model.is_none());
}

#[tokio::test]
async fn build_review_context_blocks_external_task_after_max_reroutes() {
    // Isolate from the real ~/.orch/config.yml (which may set a different
    // workflow.max_attempts) so the default of 3 reroutes is deterministic.
    let temp_home = TempDir::new().unwrap();
    let orch_dir = temp_home.path().join(".orch");
    fs::create_dir_all(&orch_dir).unwrap();
    let old_orch_home = env::var_os("ORCH_HOME");
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("ORCH_HOME", &orch_dir) };

    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let repo = "owner/repo";
    let ext = no_code_review_task("3618");
    store.ensure_external_task(repo, &ext).await.unwrap();

    let backend = Arc::new(NoopBackend);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        repo.to_string(),
    ));
    let router = Arc::new(RwLock::new(Router::new(RouterConfig::default())));

    // Default max_reroute_attempts is 3 — exhaust it.
    for _ in 0..3 {
        let phase = build_review_context(&ext, repo, &router, &task_manager, &store)
            .await
            .unwrap();
        if let ReviewPhase::EarlyReturn(ReviewDecision::Blocked(_)) = phase {
            break;
        }
    }

    let stored = store
        .get_by_external_id(repo, &ext.id.0)
        .await
        .unwrap()
        .expect("task must still exist");
    assert_eq!(stored.status, TaskStatus::Blocked);

    if let Some(old) = old_orch_home {
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        unsafe { env::set_var("ORCH_HOME", old) };
    } else {
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        unsafe { env::remove_var("ORCH_HOME") };
    }
}

#[tokio::test]
async fn build_review_context_marks_internal_task_done_with_no_code() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let repo = "owner/repo";
    let internal_id = store
        .create_internal(repo, "Daily retrospective", "", "job", "job-1", None)
        .await
        .unwrap();
    let ext_id = format!("internal:{internal_id}");
    let ext = no_code_review_task(&ext_id);

    let backend = Arc::new(NoopBackend);
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        repo.to_string(),
    ));
    let router = Arc::new(RwLock::new(Router::new(RouterConfig::default())));

    let phase = build_review_context(&ext, repo, &router, &task_manager, &store)
        .await
        .unwrap();

    assert!(matches!(
        phase,
        ReviewPhase::EarlyReturn(ReviewDecision::Skipped)
    ));
    let stored = store.get(internal_id).await.unwrap();
    assert_eq!(stored.status, TaskStatus::Done);
}

#[test]
fn no_code_reroute_skip_check_rejects_agent_stream_disconnects() {
    let last_error =
        "codex failed: Reconnecting... (stream disconnected before completion: Broken pipe)";
    assert!(!should_skip_no_code_reroute_increment(last_error));
}

// ── review fallback rate-limit/auth scan is bounded, not full-text ──────
//
// Regression test for a review transcript whose unparseable fallback text
// is a long report/diff quoting a status table (`outcome` column values
// like `rate_limit`, `failed`, `timeout`). Scanning the full text falsely
// classified the review run as rate-limited even though the review agent
// never actually hit a rate limit.

#[test]
fn review_fallback_scan_ignores_rate_limit_word_buried_in_long_report() {
    let padding = "x".repeat(10_000);
    let text_for_review = format!(
        "{padding}\n+| codex | gpt-5.4 | failed | 1 |\n+| codex | gpt-5.5 | failed / aborted / (empty) | 1 each |\n+| kimi | opus | rate_limit | 1 |\n{padding}"
    );

    let scan_head = runner::agents::patterns::safe_head(
        &text_for_review,
        runner::agents::patterns::RATE_LIMIT_SCAN_HEAD_BYTES,
    );
    let scan_tail = runner::agents::patterns::safe_tail(
        &text_for_review,
        runner::agents::patterns::RATE_LIMIT_SCAN_TAIL_BYTES,
    );

    assert!(runner::agents::patterns::detect_rate_limit(scan_head).is_none());
    assert!(runner::agents::patterns::detect_rate_limit(scan_tail).is_none());
}

#[test]
fn review_fallback_scan_still_detects_real_rate_limit_at_tail() {
    let padding = "x".repeat(10_000);
    let text_for_review = format!("{padding}\nError: rate limit exceeded, please try again later");

    let scan_tail = runner::agents::patterns::safe_tail(
        &text_for_review,
        runner::agents::patterns::RATE_LIMIT_SCAN_TAIL_BYTES,
    );

    assert!(runner::agents::patterns::detect_rate_limit(scan_tail).is_some());
}

#[tokio::test]
async fn recover_missing_review_worktree_recreates_worktree() {
    let temp_home = TempDir::new().unwrap();
    let orch_dir = temp_home.path().join(".orch");
    fs::create_dir_all(&orch_dir).unwrap();
    let old_orch_home = env::var_os("ORCH_HOME");
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("ORCH_HOME", &orch_dir) };

    let project_dir = temp_home.path().join("project");
    fs::create_dir_all(&project_dir).unwrap();
    git(&project_dir, &["init", "-b", "main"]);
    git(&project_dir, &["config", "user.name", "Test User"]);
    git(&project_dir, &["config", "user.email", "test@example.com"]);
    fs::write(project_dir.join("README.md"), "hello").unwrap();
    git(&project_dir, &["add", "README.md"]);
    git(&project_dir, &["commit", "-m", "init"]);

    fs::write(
        orch_dir.join("config.yml"),
        format!("projects:\n  - {}\n", project_dir.display()),
    )
    .unwrap();
    fs::write(project_dir.join(".orch.yml"), "gh:\n  repo: owner/repo\n").unwrap();

    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let repo = "owner/repo";
    store
        .create_internal(repo, "Fix review recovery", "", "review", "review-1", None)
        .await
        .unwrap();
    let ext = ExternalTask {
        id: ExternalId("1".to_string()),
        title: "Fix review recovery".to_string(),
        body: String::new(),
        state: "open".to_string(),
        labels: vec![],
        author: "tester".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: "https://example.com/issues/1".to_string(),
    };
    store.ensure_external_task(repo, &ext).await.unwrap();

    let recovered = recover_missing_review_worktree(&ext, repo, &store)
        .await
        .unwrap();

    assert!(recovered.work_dir.exists());
    assert!(recovered.branch.starts_with("gh-issue-1-"));

    if let Some(old) = old_orch_home {
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        unsafe { env::set_var("ORCH_HOME", old) };
    } else {
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        unsafe { env::remove_var("ORCH_HOME") };
    }
}

// ── count_ahead_commits ─────────────────────────────────────────────────

#[tokio::test]
async fn count_ahead_commits_returns_zero_when_no_ahead_commits() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();

    // Create a bare repo to act as the 'origin' remote
    let bare = temp.path().join("bare.git");
    fs::create_dir_all(&bare).unwrap();
    process::Command::new("git")
        .args(["init", "--bare"])
        .current_dir(&bare)
        .output()
        .unwrap();

    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "user.email", "test@test.com"]);
    fs::write(dir.join("f"), "a").unwrap();
    git(dir, &["add", "f"]);
    git(dir, &["commit", "-m", "init"]);
    process::Command::new("git")
        .args(["remote", "add", "origin", bare.to_str().unwrap()])
        .current_dir(dir)
        .output()
        .unwrap();
    process::Command::new("git")
        .args(["push", "origin", "main"])
        .current_dir(dir)
        .output()
        .unwrap();

    // origin/main now exists and HEAD is at the same commit — zero ahead
    let result = count_ahead_commits(dir, "main").await;
    assert!(
        result.is_ok(),
        "expected Ok when origin/main exists, got {result:?}"
    );
    assert_eq!(
        result.unwrap(),
        0,
        "expected 0 commits ahead when branch matches origin/main"
    );
}

#[tokio::test]
async fn count_ahead_commits_returns_error_when_remote_ref_missing() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "user.email", "test@test.com"]);
    fs::write(dir.join("f"), "a").unwrap();
    git(dir, &["add", "f"]);
    git(dir, &["commit", "-m", "init"]);
    // No remote 'origin' configured — rev-list will fail
    let result = count_ahead_commits(dir, "main").await;
    assert!(
        result.is_err(),
        "expected Err when origin/main does not exist, got {result:?}"
    );
    let err = result.unwrap_err();
    assert!(
        err.contains("git rev-list exited"),
        "error should mention exit failure, got: {err}"
    );
}

#[tokio::test]
async fn count_ahead_commits_returns_error_when_git_cannot_start() {
    // Use a path that is not a git repo — git will fail with non-zero exit
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    // Not a git repo at all
    let result = count_ahead_commits(dir, "main").await;
    assert!(
        result.is_err(),
        "expected Err when directory is not a git repo, got {result:?}"
    );
}

// ── review cooldown escalation (issue #3314) ────────────────────────────

/// Review `InvalidResponse` applies the short review output backoff: 5 minutes first
#[serial_test::serial(cooldown_state)]
#[tokio::test]
async fn review_invalid_response_applies_short_model_cooldown() {
    cooldown::reset_global_state().await;
    let agent = "opencode-3314-review-invalid";
    let model = "opencode/north-mini-code-free";
    let key = format!("{agent}:{model}");

    assert!(
        !cooldown::is_model_in_cooldown(agent, model),
        "model should not be in cooldown before record_review_agent_failure"
    );

    let err = runner::agents::AgentError::InvalidResponse {
        raw: "empty review output".to_string(),
    };
    record_review_agent_failure("test-3314-a", agent, Some(model), &err).await;

    let now = chrono::Utc::now().timestamp();
    let until =
        cooldown::cooldown_until(&key).expect("review InvalidResponse should set model cooldown");
    let remaining = until.saturating_sub(now);
    let base = cooldown::TRANSIENT_MODEL_BACKOFF_BASE_SECS;
    assert!(
        (base - 30..=base).contains(&remaining),
        "first unusable review output must cool the model for 5 minutes, got {remaining}s"
    );
}

/// A review `ModelUnavailable` that does not say the model is gone gets the generic backoff
#[serial_test::serial(cooldown_state)]
#[tokio::test]
async fn review_model_unavailable_transient_applies_short_cooldown() {
    cooldown::reset_global_state().await;
    let agent = "test-review-unavailable-transient";
    let model = "test-model";
    let key = format!("{agent}:{model}");

    let err = runner::agents::AgentError::ModelUnavailable {
        message: "model is temporarily overloaded".to_string(),
        model: model.to_string(),
    };
    record_review_agent_failure("test-unavailable-a", agent, Some(model), &err).await;

    let now = chrono::Utc::now().timestamp();
    let until =
        cooldown::cooldown_until(&key).expect("review ModelUnavailable should set model cooldown");
    let remaining = until.saturating_sub(now);
    let base = cooldown::BACKOFF_BASE_SECS;
    assert!(
        (base - 30..=base).contains(&remaining),
        "a non-permanent ModelUnavailable must get the 5 minute base, got {remaining}s"
    );
}

/// A review `ModelUnavailable` that says "model not found" gets the persistent backoff
#[serial_test::serial(cooldown_state)]
#[tokio::test]
async fn review_model_not_found_applies_persistent_cooldown() {
    cooldown::reset_global_state().await;
    let agent = "test-review-unavailable-gone";
    let model = "test-model";
    let key = format!("{agent}:{model}");

    let err = runner::agents::AgentError::ModelUnavailable {
        message: "Model not found: test-model".to_string(),
        model: model.to_string(),
    };
    record_review_agent_failure("test-unavailable-b", agent, Some(model), &err).await;

    let now = chrono::Utc::now().timestamp();
    let until =
        cooldown::cooldown_until(&key).expect("review model not found should set model cooldown");
    let remaining = until.saturating_sub(now);
    let base = cooldown::PERSISTENT_MODEL_BACKOFF_BASE_SECS;
    assert!(
        (base - 30..=base).contains(&remaining),
        "model not found must get the 4 hour persistent base, got {remaining}s"
    );
}

/// Repeated unusable review output escalates to 15 minutes and stays there
#[serial_test::serial(cooldown_state)]
#[tokio::test]
async fn review_output_cooldown_caps_at_fifteen_minutes() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    cooldown::init_cooldown_store(store).await;
    let agent = "opencode-review-output-cap";
    let model = "opencode/north-mini-code-free";
    let key = format!("{agent}:{model}");
    let base = cooldown::TRANSIENT_MODEL_BACKOFF_BASE_SECS;
    let max = cooldown::TRANSIENT_MODEL_BACKOFF_MAX_SECS;

    for (i, expected) in [base, max, max].into_iter().enumerate() {
        let err = runner::agents::AgentError::InvalidResponse {
            raw: format!("failed to parse review response {i}"),
        };
        record_review_agent_failure("test-review-output-cap", agent, Some(model), &err).await;
        let now = chrono::Utc::now().timestamp();
        let remaining = cooldown::cooldown_until(&key)
            .expect("unusable review output should set model cooldown")
            .saturating_sub(now);
        assert!(
            (expected - 30..=expected).contains(&remaining),
            "failure {} should cool the model for {expected}s, got {remaining}s",
            i + 1
        );
    }
}

/// Regression test for issue #3551: a billing-cycle-exhaustion message caught by
/// the review module's *inline text scan* (`detect_rate_limit` on raw review
/// output, used when the review process exits without a clean `AgentError`) must
/// get the same treatment as the `AgentError`-based failure path fixed by #3530 —
/// not a hardcoded `"rate_limit"` outcome plus a generic short agent cooldown.
#[serial_test::serial(cooldown_state)]
#[tokio::test]
async fn review_inline_scan_billing_cycle_message_gets_model_scoped_cooldown() {
    cooldown::reset_global_state().await;
    let agent = "kimi-3551-review-inline";
    let model = "opus";
    let key = format!("{agent}:{model}");

    // Verbatim text from the observed kimi failure (issue #3551).
    let text_for_review = "kimi rate limit: Failed to authenticate. API Error: 403 \
            You've reached your usage limit for this billing cycle. Your quota will be \
            refreshed next cycle.";

    let scan_tail = runner::agents::patterns::safe_tail(
        text_for_review,
        runner::agents::patterns::RATE_LIMIT_SCAN_TAIL_BYTES,
    );
    let err = runner::agents::patterns::detect_rate_limit(scan_tail)
        .expect("billing-cycle message must still be caught by the rate-limit scan");

    assert_eq!(
        outcome_for_agent_error(&err),
        "billing_cycle_exhausted",
        "inline-scanned billing-cycle message must not be stored as plain rate_limit"
    );

    record_review_agent_failure("test-3551", agent, Some(model), &err).await;

    let now = chrono::Utc::now().timestamp();
    let until = cooldown::cooldown_until(&key)
        .expect("billing-cycle exhaustion must set a model-scoped cooldown");
    let remaining = until.saturating_sub(now);
    // Known model + BillingCycleExhausted routes to record_persistent_model_failure
    // (~4h base, 7d cap), not the agent-wide record_credit_exhaustion (~24h) path —
    // the quota is scoped to this model, so unrelated models on the same agent must
    // stay routable.
    assert!(
        remaining >= cooldown::PERSISTENT_MODEL_BACKOFF_BASE_SECS - 30,
        "billing-cycle exhaustion for a known model must apply the ~4h persistent \
             model cooldown, got {remaining}s — may have used the generic short agent \
             cooldown instead"
    );
}

// ── review success resets failure_count (issue #3478) ──────────────────

fn test_review_task(id: &str) -> ExternalTask {
    ExternalTask {
        id: ExternalId(id.to_string()),
        title: "Test review task".to_string(),
        body: String::new(),
        state: "open".to_string(),
        labels: vec![],
        author: "tester".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: "https://example.com/issues/1".to_string(),
    }
}

fn test_review_context(agent: &str, model: &str, work_dir: &Path) -> ReviewContext {
    ReviewContext {
        store_id: None,
        had_prev_push_failure: false,
        review_attempt: 1,
        worktree_path: work_dir.to_path_buf(),
        branch_name: "gh-issue-1-test".to_string(),
        pr_number: 1,
        head_sha: None,
        default_branch: "main".to_string(),
        review_agent: agent.to_string(),
        review_model: Some(model.to_string()),
        review_task_id: "review-1".to_string(),
        review_attempt_dir: work_dir.to_path_buf(),
        output_file: work_dir.join("output.txt"),
        invocation: runner::agent::AgentInvocation {
            agent: agent.to_string(),
            model: Some(model.to_string()),
            reasoning_effort: None,
            work_dir: work_dir.to_path_buf(),
            system_prompt: String::new(),
            agent_message: String::new(),
            task_id: "review-1".to_string(),
            disallowed_tools: vec![],
            git_author_name: "Test".to_string(),
            git_author_email: "test@example.com".to_string(),
            output_file: work_dir.join("output.txt"),
            timeout_seconds: 0,
            repo: "owner/repo".to_string(),
            attempt: 1,
            sandbox: false,
            main_project_dir: work_dir.to_path_buf(),
            branch: String::new(),
        },
        same_agent_fallback: false,
    }
}

fn test_parsed_review(decision: ReviewDecision) -> ParsedReview {
    ParsedReview {
        run_id: None,
        exit_code: 0,
        stderr: String::new(),
        raw_output: String::new(),
        text_for_review: String::new(),
        token_usage: RunTokenUsage::default(),
        review_notes_for_comment: String::new(),
        decision,
    }
}

fn write_review_output(dir: &Path, stdout: &str, exit_code: i32) {
    fs::write(dir.join("output.txt"), stdout).unwrap();
    fs::write(dir.join("exit.txt"), exit_code.to_string()).unwrap();
    fs::write(dir.join("stderr.txt"), "").unwrap();
}

/// A kimi review that exits mid-thought (no result envelope, last content is a
/// thinking block) must be stored as incomplete and must not apply the 4h
/// persistent model cooldown (issue #3734).
#[serial_test::serial(cooldown_state)]
#[tokio::test]
async fn parse_review_output_mid_thought_is_incomplete_without_persistent_cooldown() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let work_dir = TempDir::new().unwrap();
    let agent = "kimi";
    let model = "opus-3734-mid-thought";
    let ctx = test_review_context(agent, model, work_dir.path());
    let redraw = "^D\u{0008}".repeat(20);
    let ndjson = format!(
        "{}\n{}\n{}\n{}\n{}",
        r#"{"type":"system","subtype":"thinking_tokens","tokens":1945}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"CI passes. Now verifying callers and the lease logic edge cases:"}],"stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"checking lease edge cases"}],"stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#,
        "\x1b[?25h",
        redraw,
    );
    write_review_output(work_dir.path(), &ndjson, 0);

    let task = test_review_task("3734");
    let phase = parse_review_output(
        &task,
        "owner/repo",
        &ctx,
        ReviewRun { run_id: None },
        &store,
    )
    .await;

    match phase {
        ReviewPhase::EarlyReturn(ReviewDecision::Failed(reason)) => {
            assert!(
                reason.contains("incomplete:"),
                "mid-thought review must be stored as incomplete, got {reason}"
            );
        }
        ReviewPhase::EarlyReturn(_) => panic!("expected Failed(incomplete)"),
        ReviewPhase::Ready(_) => panic!("expected Failed(incomplete), got Ready"),
    }
    assert!(
        !cooldown::is_model_in_cooldown(agent, model),
        "mid-thought incomplete must not apply persistent model cooldown"
    );
}

/// A review ended by the shell `timeout` (exit 124) with no decision is a timeout,
/// not a parse error, and puts no model in cooldown (issue #3942)
#[serial_test::serial(cooldown_state)]
#[tokio::test]
async fn parse_review_output_exit_124_is_timeout_without_cooldown() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let work_dir = TempDir::new().unwrap();
    let agent = "claude";
    let model = "opus-3942-timeout";
    let ctx = test_review_context(agent, model, work_dir.path());
    let ndjson = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"`shipcheck run` is still going. I am waiting on that local CI run before I decide."}]}}"#;
    write_review_output(work_dir.path(), ndjson, 124);

    let task = test_review_task("3942");
    let phase = parse_review_output(
        &task,
        "owner/repo",
        &ctx,
        ReviewRun { run_id: None },
        &store,
    )
    .await;

    match phase {
        ReviewPhase::EarlyReturn(ReviewDecision::Failed(reason)) => {
            assert_eq!(reason, "timeout");
        }
        ReviewPhase::EarlyReturn(_) => panic!("expected Failed(timeout)"),
        ReviewPhase::Ready(_) => panic!("expected Failed(timeout), got Ready"),
    }
    assert!(!cooldown::is_model_in_cooldown(agent, model));
}

/// A tool_use as the last assistant content with no result envelope is the
/// same incomplete shape as a thinking block (issue #3734).
#[serial_test::serial(cooldown_state)]
#[tokio::test]
async fn parse_review_output_tool_use_without_envelope_is_incomplete() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let work_dir = TempDir::new().unwrap();
    let agent = "kimi";
    let model = "opus-3734-tool-use";
    let ctx = test_review_context(agent, model, work_dir.path());
    let ndjson = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"src/lib.rs"}}]}}"#;
    write_review_output(work_dir.path(), ndjson, 0);

    let task = test_review_task("3734-tool");
    let phase = parse_review_output(
        &task,
        "owner/repo",
        &ctx,
        ReviewRun { run_id: None },
        &store,
    )
    .await;

    match phase {
        ReviewPhase::EarlyReturn(ReviewDecision::Failed(reason)) => {
            assert!(
                reason.contains("incomplete:"),
                "tool_use without envelope must be incomplete, got {reason}"
            );
        }
        ReviewPhase::EarlyReturn(_) => panic!("expected Failed(incomplete)"),
        ReviewPhase::Ready(_) => panic!("expected Failed(incomplete), got Ready"),
    }
    assert!(!cooldown::is_model_in_cooldown(agent, model));
}

/// A finished result envelope whose text is not a review decision still
/// gets the short format-failure cooldown.
#[serial_test::serial(cooldown_state)]
#[tokio::test]
async fn parse_review_output_unparseable_envelope_applies_short_cooldown() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let work_dir = TempDir::new().unwrap();
    let agent = "kimi";
    let model = "opus-3734-format";
    let ctx = test_review_context(agent, model, work_dir.path());
    let ndjson = concat!(
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"I have finished looking at the code."}]}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","is_error":false,"result":"I have finished looking at the code."}"#,
    );
    write_review_output(work_dir.path(), ndjson, 0);

    let task = test_review_task("3734-format");
    let phase = parse_review_output(
        &task,
        "owner/repo",
        &ctx,
        ReviewRun { run_id: None },
        &store,
    )
    .await;

    match phase {
        ReviewPhase::EarlyReturn(ReviewDecision::Failed(reason)) => {
            assert!(
                reason.contains("parse error"),
                "finished unparseable envelope must stay a parse error, got {reason}"
            );
        }
        ReviewPhase::EarlyReturn(_) => panic!("expected Failed(parse error)"),
        ReviewPhase::Ready(_) => panic!("expected Failed(parse error), got Ready"),
    }
    let now = chrono::Utc::now().timestamp();
    let remaining = cooldown::cooldown_until(&format!("{agent}:{model}"))
        .expect("finished format failure must apply a model cooldown")
        .saturating_sub(now);
    assert!(
        remaining <= cooldown::TRANSIENT_MODEL_BACKOFF_BASE_SECS,
        "finished format failure must apply the 5 minute cooldown, got {remaining}s"
    );
}

/// Missing result envelope with a review decision in assistant text uses
/// the assistant-text extractor, not MiniMax status=done synthesis.
#[serial_test::serial(cooldown_state)]
#[tokio::test]
async fn parse_review_output_missing_envelope_uses_assistant_decision() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let work_dir = TempDir::new().unwrap();
    let agent = "kimi";
    let model = "opus-3734-assistant";
    let ctx = test_review_context(agent, model, work_dir.path());
    let ndjson = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"{\"decision\":\"approve\",\"notes\":\"LGTM\",\"issues\":[]}"}]}}"#;
    write_review_output(work_dir.path(), ndjson, 0);

    let task = test_review_task("3734-asst");
    let phase = parse_review_output(
        &task,
        "owner/repo",
        &ctx,
        ReviewRun { run_id: None },
        &store,
    )
    .await;

    match phase {
        ReviewPhase::Ready(parsed) => {
            assert!(matches!(parsed.decision, ReviewDecision::Approve { .. }));
        }
        ReviewPhase::EarlyReturn(_) => {
            panic!("expected Ready(Approve) from assistant text, got EarlyReturn")
        }
    }
    assert!(!cooldown::is_model_in_cooldown(agent, model));
}

/// MiniMax/Kimi parse_response synthesizes status=done from any assistant
/// text. That task-path fallback must not be treated as a review approval.
#[serial_test::serial(cooldown_state)]
#[tokio::test]
async fn parse_review_output_does_not_treat_task_done_synthesis_as_approval() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let work_dir = TempDir::new().unwrap();
    let agent = "kimi";
    let model = "opus-3734-done";
    let ctx = test_review_context(agent, model, work_dir.path());
    let ndjson = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"analyzing the diff"}]}}"#;
    write_review_output(work_dir.path(), ndjson, 0);

    let task = test_review_task("3734-done");
    let phase = parse_review_output(
        &task,
        "owner/repo",
        &ctx,
        ReviewRun { run_id: None },
        &store,
    )
    .await;

    match phase {
        ReviewPhase::EarlyReturn(ReviewDecision::Failed(reason)) => {
            assert!(
                reason.contains("parse error"),
                "unfinished prose without a decision must not approve, got {reason}"
            );
        }
        ReviewPhase::Ready(_) => {
            panic!("task-path status=done synthesis must not become a review approval")
        }
        ReviewPhase::EarlyReturn(_) => panic!("expected Failed(parse error)"),
    }
}

/// Regression test for issue #3478: a successful review run must reset
/// `failure_count:{agent}:{model}`, mirroring the runner's task-agent
/// success path, for generic/transient failures (not the persistent-model
/// path — see issue #3571, which deliberately gives persistent failures
/// their own counter that a success does NOT reset).
#[serial_test::serial(cooldown_state)]
#[tokio::test]
async fn finalize_review_run_success_resets_generic_model_failure_count() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    cooldown::init_cooldown_store(Arc::clone(&store)).await;

    let agent = "test-3478-agent";
    let model = "test-3478-model";
    let failure_key = format!("failure_count:{agent}:{model}");

    // AgentFailed is a generic/transient error, routed to record_model_failure
    // (not record_persistent_model_failure), so it should still be reset by
    // an intervening success.
    let err = runner::agents::AgentError::AgentFailed {
        message: "agent reported a generic failure".to_string(),
    };
    record_review_agent_failure("task-3478-a", agent, Some(model), &err).await;
    time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        store.kv_get(&failure_key).await.unwrap().as_deref(),
        Some("1"),
        "sanity check: failure should have incremented the persisted count"
    );

    let work_dir = TempDir::new().unwrap();
    let ctx = test_review_context(agent, model, work_dir.path());
    let parsed = test_parsed_review(ReviewDecision::Approve { issues: vec![] });
    let task = test_review_task("1");

    finalize_review_run(&task, &ctx, &parsed, &store).await;
    time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        store.kv_get(&failure_key).await.unwrap().as_deref(),
        Some("0"),
        "successful review must reset failure_count:{{agent}}:{{model}} (issue #3478)"
    );
}

/// Regression test for issue #3478: successful reviews interleaved between
/// isolated generic failures must keep each failure's backoff at the base
/// duration instead of escalating, since the previous success reset the
/// counter. Uses AgentFailed (generic/transient), not ModelUnavailable
/// (persistent-model path, see issue #3571).
#[serial_test::serial(cooldown_state)]
#[tokio::test]
async fn finalize_review_run_interleaved_success_keeps_generic_cooldown_at_base() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    cooldown::init_cooldown_store(Arc::clone(&store)).await;

    let agent = "test-3478-interleave-agent";
    let model = "test-3478-interleave-model";
    let cooldown_key = format!("{agent}:{model}");

    let work_dir = TempDir::new().unwrap();
    let ctx = test_review_context(agent, model, work_dir.path());
    let task = test_review_task("2");

    for i in 0..4 {
        let err = runner::agents::AgentError::AgentFailed {
            message: format!("failure {i}"),
        };
        record_review_agent_failure("task-3478-b", agent, Some(model), &err).await;

        let now = chrono::Utc::now().timestamp();
        let until = cooldown::cooldown_until(&cooldown_key).expect("failure should set cooldown");
        let remaining = until.saturating_sub(now);

        assert!(
            remaining <= cooldown::BACKOFF_BASE_SECS + 60,
            "failure #{} must stay at base backoff since the prior success reset the \
                 counter (issue #3478); got {remaining}s, base is {}s — an escalating value \
                 here means review successes are not resetting failure_count",
            i + 1,
            cooldown::BACKOFF_BASE_SECS
        );

        // A successful review interleaves between each failure, mirroring the
        // real-world pattern from issue #3478 (reviews for other tasks succeed
        // in between isolated failures for this model).
        let parsed = test_parsed_review(ReviewDecision::Approve { issues: vec![] });
        finalize_review_run(&task, &ctx, &parsed, &store).await;
        time::sleep(Duration::from_millis(20)).await;
    }
}

/// Regression test for issue #3571: unlike generic failures, persistent
/// model failures (e.g. ModelUnavailable) must NOT be
/// reset by an intervening successful review. Otherwise a model that fails
/// structurally every so often — but succeeds most of the time — never
/// escalates past the 4h base cooldown and stays in rotation indefinitely.
#[serial_test::serial(cooldown_state)]
#[tokio::test]
async fn finalize_review_run_success_does_not_reset_persistent_model_failure_count() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    cooldown::init_cooldown_store(Arc::clone(&store)).await;

    let agent = "test-3571-agent";
    let model = "test-3571-model";
    let cooldown_key = format!("{agent}:{model}");

    let work_dir = TempDir::new().unwrap();
    let ctx = test_review_context(agent, model, work_dir.path());
    let task = test_review_task("3");

    // First persistent model failure -> base 4h cooldown.
    let err = runner::agents::AgentError::ModelUnavailable {
        message: "model not found".to_string(),
        model: model.to_string(),
    };
    record_review_agent_failure("task-3571-a", agent, Some(model), &err).await;
    time::sleep(Duration::from_millis(20)).await;

    // A successful review for the same model interleaves, exactly like the
    // real-world pattern from issue #3571.
    let parsed = test_parsed_review(ReviewDecision::Approve { issues: vec![] });
    finalize_review_run(&task, &ctx, &parsed, &store).await;
    time::sleep(Duration::from_millis(20)).await;

    // Second persistent failure must escalate from count 2 (~12h), not
    // reset back to the 4h base, despite the intervening success.
    let err2 = runner::agents::AgentError::ModelUnavailable {
        message: "model not found again".to_string(),
        model: model.to_string(),
    };
    record_review_agent_failure("task-3571-b", agent, Some(model), &err2).await;
    time::sleep(Duration::from_millis(20)).await;

    let now = chrono::Utc::now().timestamp();
    let until =
        cooldown::cooldown_until(&cooldown_key).expect("second failure should set cooldown");
    let remaining = until.saturating_sub(now);

    assert!(
        remaining > cooldown::PERSISTENT_MODEL_BACKOFF_BASE_SECS,
        "second persistent model failure should escalate past the 4h base despite an \
             intervening success (issue #3571); got {remaining}s, base is {}s",
        cooldown::PERSISTENT_MODEL_BACKOFF_BASE_SECS
    );
}

fn app_for_test(server: &wiremock::MockServer) -> Arc<AppAuth> {
    Arc::new(AppAuth::new(
        "12345".to_string(),
        app::test_pem().to_string(),
        server.uri(),
    ))
}

async fn mount_installed(server: &wiremock::MockServer, repo: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/{repo}/installation")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 42})))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/app/installations/42/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "token": "inst-1",
            "expires_at": "2099-01-01T00:00:00Z",
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn app_mode_falls_back_to_comments_where_the_app_is_not_installed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/org/b/installation"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let app = app_for_test(&server);

    assert!(
        review_app_for(ReviewMode::App, Some(Arc::clone(&app)), "org/b")
            .await
            .is_none()
    );
    // Comment mode never asks GitHub, the expect(1) above counts the only lookup
    assert!(
        review_app_for(ReviewMode::Comment, Some(Arc::clone(&app)), "org/b")
            .await
            .is_none()
    );
    assert!(
        review_app_for(ReviewMode::App, None, "org/b")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn the_check_run_starts_and_completes_on_the_reviewed_sha() {
    let server = MockServer::start().await;
    mount_installed(&server, "o/r").await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/check-runs"))
        .and(body_partial_json(serde_json::json!({
            "name": "orch/review", "head_sha": "abc123", "status": "in_progress",
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/check-runs"))
        .and(body_partial_json(serde_json::json!({
            "name": "orch/review", "head_sha": "abc123", "status": "completed",
            "conclusion": "failure", "output": {"summary": "fix it"},
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 2})))
        .expect(1)
        .mount(&server)
        .await;
    let app = app_for_test(&server);
    let gh = GhHttp::for_test(server.uri(), Some(Arc::clone(&app)));
    let decision = ReviewDecision::RequestChanges {
        notes: "fix it".to_string(),
        issues: vec![],
    };

    post_review_started(Some(&app), &gh, "o/r", Some("abc123")).await;
    post_review_decision(Some(&app), &gh, "o/r", Some("abc123"), &decision, "fix it").await;
    let requests = server.received_requests().await.expect("requests");
    assert!(
        requests
            .iter()
            .all(|req| !req.url.path().contains("/statuses/"))
    );
}

#[tokio::test]
async fn issues_on_diff_lines_go_inline_and_the_rest_stay_in_the_body() {
    let server = MockServer::start().await;
    mount_installed(&server, "o/r").await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/pulls/9/files"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"filename": "src/a.rs", "patch": "@@ -1,2 +10,3 @@\n ctx\n+new\n+new"},
        ])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls/9/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/9/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 2})))
        .mount(&server)
        .await;
    let issue = |file: &str, line: Option<u32>| ReviewIssue {
        file: file.to_string(),
        line,
        severity: "error".to_string(),
        description: "wrong".to_string(),
    };
    let decision = ReviewDecision::RequestChanges {
        notes: "fix".to_string(),
        issues: vec![
            issue("src/a.rs", Some(11)),
            issue("src/a.rs", Some(50)),
            issue("src/b.rs", Some(3)),
            issue("src/a.rs", None),
        ],
    };
    let app = app_for_test(&server);
    let gh = GhHttp::for_test(server.uri(), Some(Arc::clone(&app)));

    publish_review(Some(&app), &gh, "o/r", 9, Some("abc123"), &decision, "body")
        .await
        .expect("comment posted");
    let requests = server.received_requests().await.expect("requests");
    let review = requests
        .iter()
        .find(|req| req.url.path().ends_with("/reviews"))
        .expect("review submitted");
    let body: serde_json::Value = serde_json::from_slice(&review.body).unwrap();
    assert_eq!(
        body["comments"],
        serde_json::json!([{"path": "src/a.rs", "line": 11, "side": "RIGHT", "body": "**error:** wrong"}])
    );
}

#[expect(
    clippy::await_holding_lock,
    reason = "the env lock must stay held across the awaited review call"
)]
#[tokio::test]
async fn without_an_app_the_review_path_posts_only_the_comment() {
    let _env = token::ENV_LOCK.lock().unwrap();
    let prev = env::var_os("GH_TOKEN");
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("GH_TOKEN", "owner-token") };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/9/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;
    let gh = GhHttp::for_test(server.uri(), None);
    let result = publish_review(
        None,
        &gh,
        "o/r",
        9,
        Some("abc123"),
        &ReviewDecision::Approve { issues: vec![] },
        "## Automated Review \u{2014} Approve\n\nok",
    )
    .await;
    match prev {
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        Some(value) => unsafe { env::set_var("GH_TOKEN", value) },
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        None => unsafe { env::remove_var("GH_TOKEN") },
    }
    result.expect("comment posted");
    let requests = server.received_requests().await.expect("requests");
    assert!(
        requests
            .iter()
            .all(|req| !req.url.path().contains("/reviews"))
    );
}

#[tokio::test]
async fn with_an_app_the_review_is_submitted_on_the_reviewed_sha() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/app/installations/42/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "token": "inst-1",
            "expires_at": "2099-01-01T00:00:00Z",
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/pulls/9/reviews"))
        .and(body_partial_json(serde_json::json!({
            "event": "REQUEST_CHANGES",
            "commit_id": "abc123",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 1})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/o/r/issues/9/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 2})))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/installation"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 42})))
        .mount(&server)
        .await;
    let app = Arc::new(AppAuth::new(
        "12345".to_string(),
        app::test_pem().to_string(),
        server.uri(),
    ));
    let gh = GhHttp::for_test(server.uri(), Some(Arc::clone(&app)));
    let decision = ReviewDecision::RequestChanges {
        notes: "fix it".to_string(),
        issues: vec![],
    };
    publish_review(
        Some(&app),
        &gh,
        "o/r",
        9,
        Some("abc123"),
        &decision,
        "## Automated Review \u{2014} Changes Requested\n\nfix it",
    )
    .await
    .expect("comment posted");
}
