// Secret redaction on posted bodies (wiremock)
mod closed_pr;
mod leak_redaction;

use super::checks::is_no_branch_protection_error;
use super::checks::merge_required_status_checks;
use super::checks::parse_classic_required_status_checks;
use super::checks::parse_ruleset_required_status_checks;
use super::checks::required_check_ruleset_payload;
use super::pulls::build_pr_issue_comments_page_query;
use super::pulls::build_pr_review_alias;
use super::pulls::connection_end_cursor;
use super::pulls::parse_behind_by;
use super::pulls::parse_commit_meta;
use crate::github::types;
use crate::github::types::GitHubIssue;
use crate::github::types::RequiredStatusCheck;
use std::env;

use super::*;
use crate::github::token::TokenResolver;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[test]
fn repo_from_url_names_the_repository_or_nothing() {
    let api = "https://api.github.com";
    assert_eq!(
        repo_from_url(&format!("{api}/repos/me/a/pulls/9")).as_deref(),
        Some("me/a")
    );
    assert_eq!(
        repo_from_url(&format!("{api}/repos/org/b?per_page=1")).as_deref(),
        Some("org/b")
    );
    assert_eq!(
        repo_from_url(&format!("{api}/repos/org/b")).as_deref(),
        Some("org/b")
    );
    assert_eq!(repo_from_url(&format!("{api}/graphql")), None);
    assert_eq!(repo_from_url(&format!("{api}/user")), None);
    assert_eq!(repo_from_url(&format!("{api}/repos/only-owner")), None);
}

#[test]
fn no_branch_protection_errors() {
    let free_plan = "GitHub API GET https://api.github.com/repos/o/r/branches/main/protection failed (403 Forbidden): {\"message\":\"Upgrade to GitHub Pro or make this repository public to enable this feature.\"}";
    assert!(is_no_branch_protection_error(free_plan));
    assert!(is_no_branch_protection_error(
        "failed (404 Not Found): {\"message\":\"Branch not protected\"}"
    ));
    assert!(!is_no_branch_protection_error(
        "failed (403 Forbidden): {\"message\":\"Resource not accessible\"}"
    ));
}

#[test]
fn required_check_ruleset_payload_targets_the_default_branch() {
    let payload = required_check_ruleset_payload(&[
        RequiredStatusCheck {
            context: "test".to_string(),
            integration_id: Some(15368),
        },
        RequiredStatusCheck {
            context: "lint".to_string(),
            integration_id: None,
        },
    ]);
    assert_eq!(payload["name"], REQUIRED_CHECK_RULESET_NAME);
    assert_eq!(payload["target"], "branch");
    assert_eq!(
        payload["conditions"]["ref_name"]["include"],
        serde_json::json!(["~DEFAULT_BRANCH"])
    );
    assert_eq!(
        payload["rules"][0]["parameters"]["strict_required_status_checks_policy"],
        true
    );
    assert_eq!(
        payload["rules"][0]["parameters"]["required_status_checks"],
        serde_json::json!([
            { "context": "test", "integration_id": 15368 },
            { "context": "lint" }
        ])
    );
}

#[test]
fn classify_ruleset_create_error_splits_retry_from_refuse() {
    assert_eq!(
        classify_ruleset_create_error(
            "GitHub API POST https://api.github.com/repos/o/r/rulesets failed (422 Unprocessable Entity): {\"message\":\"Name has already been taken\"}"
        ),
        RulesetCreateKind::NameTaken
    );
    assert_eq!(
        classify_ruleset_create_error(
            "failed (422 Unprocessable Entity): Status check required to be present is not found"
        ),
        RulesetCreateKind::CheckNeverRun
    );
    assert_eq!(
        classify_ruleset_create_error("failed (422): ci/test was not a valid check"),
        RulesetCreateKind::CheckNeverRun
    );
    assert_eq!(
        classify_ruleset_create_error(
            "GitHub API POST /rulesets failed (403 Forbidden): {\"message\":\"Upgrade to GitHub Pro or make this repository public to enable this feature.\"}"
        ),
        RulesetCreateKind::Refused
    );
    assert_eq!(
        classify_ruleset_create_error("failed (500 Internal Server Error): boom"),
        RulesetCreateKind::Retryable
    );
    assert_eq!(
        classify_ruleset_create_error(
            "GitHub API POST /rulesets failed (429 Too Many Requests): {\"message\":\"API rate limit exceeded\"}"
        ),
        RulesetCreateKind::Retryable
    );
    assert_eq!(
        classify_ruleset_create_error(
            "HTTP request failed to send (transport error): connection reset by peer"
        ),
        RulesetCreateKind::Retryable
    );
    assert_eq!(
        classify_ruleset_create_error(
            "error sending request for url (https://api.github.com/repos/o/r/rulesets): operation timed out"
        ),
        RulesetCreateKind::Retryable
    );
    assert_eq!(
        classify_ruleset_create_error(
            "failed (422 Unprocessable Entity): {\"message\":\"Ruleset validation failed for rule 401\"}"
        ),
        RulesetCreateKind::Other
    );
    assert_eq!(
        classify_ruleset_create_error(
            "GitHub API POST /rulesets failed (401 Unauthorized): {\"message\":\"Bad credentials\"}"
        ),
        RulesetCreateKind::Refused
    );
}

#[test]
fn parse_classic_required_status_checks_reads_checks_app_id() {
    let value = serde_json::json!({
        "required_status_checks": {
            "contexts": ["ci/test"],
            "checks": [{ "context": "ci/test", "app_id": 42 }]
        }
    });
    assert_eq!(
        parse_classic_required_status_checks(&value),
        vec![RequiredStatusCheck {
            context: "ci/test".to_string(),
            integration_id: Some(42),
        }]
    );
}

#[test]
fn parse_classic_required_status_checks_falls_back_to_contexts() {
    let value = serde_json::json!({
        "required_status_checks": {
            "contexts": ["ci/test", "lint"]
        }
    });
    assert_eq!(
        parse_classic_required_status_checks(&value),
        vec![
            RequiredStatusCheck {
                context: "ci/test".to_string(),
                integration_id: None,
            },
            RequiredStatusCheck {
                context: "lint".to_string(),
                integration_id: None,
            }
        ]
    );
}

#[test]
fn parse_classic_required_status_checks_empty_when_unprotected() {
    let checks = parse_classic_required_status_checks(&serde_json::json!({}));
    assert!(checks.is_empty(), "{checks:?}");
}

#[test]
fn parse_ruleset_required_status_checks_reads_integration_id() {
    let rules = vec![
        serde_json::json!({ "type": "pull_request" }),
        serde_json::json!({
            "type": "required_status_checks",
            "parameters": {
                "required_status_checks": [
                    { "context": "ci/test", "integration_id": 42 },
                    { "context": "lint" }
                ]
            }
        }),
    ];
    assert_eq!(
        parse_ruleset_required_status_checks(&rules),
        vec![
            RequiredStatusCheck {
                context: "ci/test".to_string(),
                integration_id: Some(42),
            },
            RequiredStatusCheck {
                context: "lint".to_string(),
                integration_id: None,
            }
        ]
    );
}

#[test]
fn parse_ruleset_required_status_checks_treats_zero_integration_id_as_any_app() {
    let rules = vec![serde_json::json!({
        "type": "required_status_checks",
        "parameters": {
            "required_status_checks": [{ "context": "ci/test", "integration_id": 0 }]
        }
    })];
    assert_eq!(
        parse_ruleset_required_status_checks(&rules),
        vec![RequiredStatusCheck {
            context: "ci/test".to_string(),
            integration_id: None,
        }]
    );
}

#[test]
fn merge_required_status_checks_dedupes_identical_pairs() {
    let classic = vec![RequiredStatusCheck {
        context: "ci/test".to_string(),
        integration_id: None,
    }];
    let ruleset = vec![
        RequiredStatusCheck {
            context: "ci/test".to_string(),
            integration_id: None,
        },
        RequiredStatusCheck {
            context: "ci/test".to_string(),
            integration_id: Some(42),
        },
    ];
    assert_eq!(
        merge_required_status_checks(classic, ruleset),
        vec![
            RequiredStatusCheck {
                context: "ci/test".to_string(),
                integration_id: None,
            },
            RequiredStatusCheck {
                context: "ci/test".to_string(),
                integration_id: Some(42),
            }
        ]
    );
}

#[test]
fn parse_commit_meta_reads_parents_in_order_and_logins() {
    let merge = serde_json::json!({
        "parents": [{"sha": "approved"}, {"sha": "base"}],
        "author": {"login": "owner"},
        "committer": {"login": "web-flow"},
    });
    let meta = parse_commit_meta(&merge);
    assert_eq!(meta.parents, vec!["approved", "base"]);
    assert_eq!(meta.author_login.as_deref(), Some("owner"));
    assert_eq!(meta.committer_login.as_deref(), Some("web-flow"));
    assert_eq!(
        parse_commit_meta(&serde_json::json!({})),
        CommitMeta::default()
    );
}

#[test]
fn parse_behind_by_reads_compare_status() {
    // A head behind the base must not merge: its checks ran against old code.
    let behind = serde_json::json!({"status": "behind", "behind_by": 3, "ahead_by": 0});
    assert_eq!(parse_behind_by(&behind).unwrap(), 3);
    let diverged = serde_json::json!({"status": "diverged", "behind_by": 1, "ahead_by": 2});
    assert_eq!(parse_behind_by(&diverged).unwrap(), 1);
    let ahead = serde_json::json!({"status": "ahead", "behind_by": 0, "ahead_by": 2});
    assert_eq!(parse_behind_by(&ahead).unwrap(), 0);
    // An unreadable response is an error, never a permissive zero.
    assert!(parse_behind_by(&serde_json::json!({"status": "ahead"})).is_err());
}

async fn spawn_truncated_body_server(body_prefix: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await;

        let declared_len = body_prefix.len() + 32;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {declared_len}\r\nconnection: close\r\n\r\n{body_prefix}"
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    });

    format!("http://{addr}")
}

async fn spawn_silent_peer() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                // Hold the TCP connection open and never write a response
                let _socket = socket;
                time::sleep(Duration::from_secs(60)).await;
            });
        }
    });
    format!("http://{addr}")
}

struct RetryAttemptsGuard {
    previous: u64,
}

impl RetryAttemptsGuard {
    fn set(attempts: u32) -> Self {
        let previous = TEST_RETRY_ATTEMPTS.swap(u64::from(attempts), Ordering::Relaxed);
        Self { previous }
    }
}

impl Drop for RetryAttemptsGuard {
    fn drop(&mut self) {
        TEST_RETRY_ATTEMPTS.store(self.previous, Ordering::Relaxed);
    }
}

#[test]
fn graphql_timeout_is_short_for_queries() {
    assert_eq!(graphql_timeout("query { viewer { login } }"), READ_TIMEOUT);
    assert_eq!(
        graphql_timeout("\n  query($id: ID!) { node(id: $id) { id } }"),
        READ_TIMEOUT
    );
    assert_eq!(
        graphql_timeout("mutation($prId: ID!) { enablePullRequestAutoMerge }"),
        REQUEST_TIMEOUT
    );
}

#[test]
fn merge_pr_body_pins_the_head_sha() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let body = merge_pr_body(sha);
    assert_eq!(body["merge_method"], "squash");
    assert_eq!(body["sha"], sha);
    assert!(is_full_commit_sha(sha));
    assert!(!is_full_commit_sha("abc"));
    assert!(!is_full_commit_sha(""));
}

fn test_client() -> GhHttp {
    GhHttp {
        client: reqwest::Client::new(),
        token_resolver: Arc::new(TokenResolver::default_env()),
        app: None,
        owner_only: false,
        cached_username: OnceLock::new(),
        api_base: None,
    }
}

/// Verify `GhHttp::new()` returns `Ok` and does not panic on construction.
///
/// This test guards against regressions where `GhHttp::new()` might panic
/// (e.g., if TLS initialisation fails or the builder API changes).
#[test]
fn gh_http_new_returns_ok() {
    let result = GhHttp::new();
    assert!(
        result.is_ok(),
        "GhHttp::new() must not panic and must return Ok; got: {:?}",
        result.err()
    );
}

#[tokio::test]
async fn read_requests_time_out_on_a_silent_peer() {
    let _retries = RetryAttemptsGuard::set(1);
    let guard = env::var_os("GH_TOKEN");
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("GH_TOKEN", "test_token") };
    let server_url = spawn_silent_peer().await;
    let gh = test_client();
    let bound = READ_TIMEOUT + Duration::from_secs(2);

    let start = Instant::now();
    let get_result = time::timeout(
        bound,
        gh.get_json::<serde_json::Value>(&format!("{server_url}/repos/o/r")),
    )
    .await;
    let get_elapsed = start.elapsed();
    let get_err = get_result
        .expect("GET should fail within the short read timeout")
        .unwrap_err()
        .to_string();
    assert!(
        get_err.contains("is_timeout=true"),
        "GET silent peer should time out, got: {get_err}"
    );
    assert!(
        get_elapsed >= Duration::from_millis(800),
        "GET should wait the read timeout, elapsed {get_elapsed:?}"
    );
    assert!(
        get_elapsed < bound,
        "GET should fail well under the 30 s client timeout, elapsed {get_elapsed:?}"
    );

    let start = Instant::now();
    let gql_result = time::timeout(
        bound,
        gh.graphql_request_to_url(
            &format!("{server_url}/graphql"),
            "query { viewer { login } }",
            &[],
            None,
        ),
    )
    .await;
    let gql_elapsed = start.elapsed();
    let gql_err = gql_result
        .expect("GraphQL query should fail within the short read timeout")
        .unwrap_err()
        .to_string();
    assert!(
        gql_err.contains("is_timeout=true"),
        "GraphQL silent peer should time out, got: {gql_err}"
    );
    assert!(
        gql_elapsed >= Duration::from_millis(800),
        "GraphQL should wait the read timeout, elapsed {gql_elapsed:?}"
    );
    assert!(
        gql_elapsed < bound,
        "GraphQL should fail well under the 30 s client timeout, elapsed {gql_elapsed:?}"
    );

    // list_blocked_by sends its GET through dependency_request, and its URL is fixed to the API host
    let start = Instant::now();
    let dep_result = time::timeout(
        bound,
        gh.dependency_request(
            &format!("{server_url}/repos/o/r/issues/1/dependencies/blocked_by"),
            reqwest::Method::GET,
            None,
            None,
        ),
    )
    .await;
    let dep_elapsed = start.elapsed();
    let dep_err = dep_result
        .expect("dependency GET should fail within the short read timeout")
        .unwrap_err()
        .to_string();
    assert!(
        dep_err.contains("is_timeout=true"),
        "dependency GET silent peer should time out, got: {dep_err}"
    );
    assert!(
        dep_elapsed >= Duration::from_millis(800),
        "dependency GET should wait the read timeout, elapsed {dep_elapsed:?}"
    );
    assert!(
        dep_elapsed < bound,
        "dependency GET should fail well under the 30 s client timeout, elapsed {dep_elapsed:?}"
    );

    match guard {
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        Some(v) => unsafe { env::set_var("GH_TOKEN", v) },
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        None => unsafe { env::remove_var("GH_TOKEN") },
    }
}

#[test]
fn batch_get_issue_states_query_includes_labels_pagination() {
    let issue_numbers = [1u64, 2, 3];

    let aliases: String = issue_numbers
        .iter()
        .map(|n| {
            format!(
                "issue{n}: issue(number: {n}) {{ state labels(first: 100) {{ nodes {{ name }} }} }}"
            )
        })
        .collect::<Vec<_>>()
        .join(" ");

    let query = format!(
        r"query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ {aliases} }} }}"
    );
    assert!(query.contains("$owner"));
    assert!(query.contains("$name"));
    assert!(query.contains("issue1: issue(number: 1)"));
    assert!(query.contains("issue3: issue(number: 3)"));
    assert!(query.contains("labels(first: 100)"));
}

#[test]
fn parse_positive_issue_number_accepts_positive_values() {
    assert_eq!(parse_positive_issue_number("42").unwrap(), 42);
}

#[test]
fn parse_positive_issue_number_rejects_zero() {
    let err = parse_positive_issue_number("0").unwrap_err().to_string();
    assert!(err.contains("invalid issue number: 0"));
}

#[test]
fn parse_positive_issue_number_rejects_internal_ids() {
    let err = parse_positive_issue_number("internal:5")
        .unwrap_err()
        .to_string();
    assert!(err.contains("invalid issue number: internal:5"));
}

#[test]
fn pr_review_batch_query_includes_page_info_for_nested_connections() {
    let query = build_pr_review_alias(42);

    assert!(query.contains("reviews(first: 50)"));
    assert!(query.contains("reviews(first: 50) {\n    nodes"));
    assert!(query.contains("pageInfo { hasNextPage }"));
    assert!(query.contains("reviewThreads(first: 50)"));
    assert!(query.contains("comments(first: 20)"));
    assert!(query.contains("pageInfo { hasNextPage endCursor }"));
    assert!(query.contains("updatedAt"));
}

#[test]
fn pr_issue_comments_page_query_uses_cursor_pagination() {
    let (query, vars) = build_pr_issue_comments_page_query("owner", "repo", 42, "cursor123");

    assert!(query.contains("pullRequest(number: $number)"));
    assert!(query.contains("comments(first: 100, after: $after)"));
    assert!(query.contains("pageInfo { hasNextPage endCursor }"));
    assert!(query.contains("updatedAt"));
    assert_eq!(vars["owner"], "owner");
    assert_eq!(vars["name"], "repo");
    assert_eq!(vars["number"], 42);
    assert_eq!(vars["after"], "cursor123");
}

#[test]
fn connection_end_cursor_only_returns_cursor_when_more_pages_exist() {
    let page_info = serde_json::json!({
        "hasNextPage": true,
        "endCursor": "cursor123"
    });
    assert_eq!(
        connection_end_cursor(Some(&page_info)),
        Some("cursor123".to_string())
    );

    let final_page = serde_json::json!({
        "hasNextPage": false,
        "endCursor": "cursor456"
    });
    assert_eq!(connection_end_cursor(Some(&final_page)), None);
}

#[test]
fn status_label_colors_match_bash_palette() {
    assert_eq!(status_label_color("status:new"), "0e8a16");
    assert_eq!(status_label_color("status:routed"), "1d76db");
    assert_eq!(status_label_color("status:in_progress"), "fbca04");
    assert_eq!(status_label_color("status:done"), "6f42c1");
    assert_eq!(status_label_color("status:blocked"), "d73a4a");
    assert_eq!(status_label_color("status:in_review"), "0075ca");
    assert_eq!(status_label_color("status:needs_review"), "e4e669");
}

#[test]
fn unknown_label_gets_default_color() {
    assert_eq!(status_label_color("agent:claude"), "c5def5");
    assert_eq!(status_label_color("enhancement"), "c5def5");
    assert_eq!(status_label_color(""), "c5def5");
}

#[test]
fn parse_link_next_finds_next_url() {
    let mut headers = header::HeaderMap::new();
    headers.insert(
        "link",
        "<https://api.github.com/repos/foo/bar/issues?page=2>; rel=\"next\", <https://api.github.com/repos/foo/bar/issues?page=5>; rel=\"last\""
            .parse()
            .unwrap(),
    );
    assert_eq!(
        parse_link_next(&headers),
        Some("https://api.github.com/repos/foo/bar/issues?page=2".to_string())
    );
}

#[test]
fn parse_link_next_none_when_no_next() {
    let mut headers = header::HeaderMap::new();
    headers.insert(
        "link",
        "<https://api.github.com/repos/foo/bar/issues?page=1>; rel=\"prev\""
            .parse()
            .unwrap(),
    );
    assert_eq!(parse_link_next(&headers), None);
}

#[test]
fn parse_link_next_none_when_missing() {
    let headers = header::HeaderMap::new();
    assert_eq!(parse_link_next(&headers), None);
}

#[test]
fn parse_link_next_none_when_malformed() {
    let mut headers = header::HeaderMap::new();
    // Malformed: no angle-bracket URL, just garbage
    headers.insert("link", "garbage; rel=\"next\"".parse().unwrap());
    assert_eq!(parse_link_next(&headers), None);
}

#[test]
fn parse_link_next_none_when_gt_before_lt() {
    let mut headers = header::HeaderMap::new();
    headers.insert("link", ">junk; rel=\"next\"; <url>".parse().unwrap());
    assert_eq!(parse_link_next(&headers), None); // should not panic
}

#[test]
fn parse_link_next_none_when_empty_url() {
    let mut headers = header::HeaderMap::new();
    headers.insert("link", "<>; rel=\"next\"".parse().unwrap());
    assert_eq!(parse_link_next(&headers), None);
}

fn make_rl(base_secs: u64, max_secs: u64) -> RateLimit {
    RateLimit {
        remaining: None,
        reset_at: None,
        backoff_until: None,
        backoff_delay: Duration::ZERO,
        backoff_base: Duration::from_secs(base_secs),
        backoff_max: Duration::from_secs(max_secs),
        throttle_threshold: 10,
    }
}

#[test]
fn rate_limit_inactive_by_default() {
    let rl = make_rl(30, 900);
    assert!(rl.is_active().is_none());
}

#[test]
fn rate_limit_backoff_activates() {
    let mut rl = make_rl(5, 60);
    rl.record_rate_limit();
    assert!(rl.is_active().is_some());
    assert_eq!(rl.backoff_delay, Duration::from_secs(5));
}

#[test]
fn rate_limit_backoff_doubles() {
    let mut rl = make_rl(5, 60);
    rl.record_rate_limit();
    assert_eq!(rl.backoff_delay, Duration::from_secs(5));
    rl.record_rate_limit();
    assert_eq!(rl.backoff_delay, Duration::from_secs(10));
    rl.record_rate_limit();
    assert_eq!(rl.backoff_delay, Duration::from_secs(20));
}

#[test]
fn rate_limit_backoff_caps_at_max() {
    let mut rl = make_rl(30, 60);
    rl.record_rate_limit(); // 30
    rl.record_rate_limit(); // 60
    rl.record_rate_limit(); // capped at 60
    assert_eq!(rl.backoff_delay, Duration::from_secs(60));
}

#[test]
fn rate_limit_success_resets() {
    let mut rl = make_rl(5, 60);
    rl.record_rate_limit();
    assert!(rl.is_active().is_some());
    rl.record_success();
    assert!(rl.is_active().is_none());
    assert_eq!(rl.backoff_delay, Duration::ZERO);
}

#[test]
fn page_cache_key_separates_types_and_queries() {
    let a = page_cache_key::<GitHubIssue>("/issues", Some(&[("state", "open")])).unwrap();
    let b = page_cache_key::<GitHubIssue>("/issues", Some(&[("state", "closed")])).unwrap();
    let c = page_cache_key::<serde_json::Value>("/issues", Some(&[("state", "open")])).unwrap();
    assert_ne!(a, b);
    assert_ne!(a, c);
    assert_eq!(
        a,
        page_cache_key::<GitHubIssue>("/issues", Some(&[("state", "open")])).unwrap()
    );
}

/// `since` is derived from the clock, so the key would never recur.
#[test]
fn page_cache_key_skips_since_filtered_requests() {
    assert!(
        page_cache_key::<GitHubIssue>(
            "/issues",
            Some(&[("state", "closed"), ("since", "2026-10-01T00:00:00Z")])
        )
        .is_none()
    );
    // Also when it arrives embedded in a Link: next URL.
    assert!(
        page_cache_key::<GitHubIssue>("/issues?since=2026-10-01T00:00:00Z&page=2", None).is_none()
    );
}

/// A limiter with no quota left and a reset an hour out: the check must return an
/// error straight away. Sleeping here is what froze the tick loop for 504s.
#[test]
fn low_quota_fails_fast_instead_of_waiting_for_reset() {
    let mut rl = make_rl(30, 900);
    rl.remaining = Some(0);
    rl.reset_at = Some(
        SystemTime::now()
            .duration_since(std_time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600,
    );
    let cell = Mutex::new(rl);

    let start = Instant::now();
    let err = check_rate_limit_backoff(&cell, "REST").unwrap_err();
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "check blocked for {:?}",
        start.elapsed()
    );
    assert!(err.to_string().contains("rate-limited"), "{err}");
}

/// Quota above the reserve is not a reason to skip the call.
#[test]
fn healthy_quota_passes_the_check() {
    let mut rl = make_rl(30, 900);
    rl.remaining = Some(4000);
    rl.reset_at = Some(u64::MAX / 2);
    let cell = Mutex::new(rl);
    assert!(check_rate_limit_backoff(&cell, "REST").is_ok());
}

#[test]
fn rate_limit_proactive_pause() {
    let mut rl = make_rl(30, 900);
    // Simulate: 0 remaining (below default threshold of 10), reset 60s in the future
    rl.remaining = Some(0);
    let future_epoch = SystemTime::now()
        .duration_since(std_time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 60;
    rl.reset_at = Some(future_epoch);
    let pause = rl.is_active();
    assert!(pause.is_some());
    // Should be roughly 60 seconds (within tolerance, +1s buffer added)
    assert!(pause.unwrap().as_secs() <= 62);
}

#[test]
fn rate_limit_proactive_pause_threshold() {
    let mut rl = make_rl(30, 900);
    // remaining=15 is above default threshold of 10 — no proactive wait
    rl.remaining = Some(15);
    let future_epoch = SystemTime::now()
        .duration_since(std_time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 60;
    rl.reset_at = Some(future_epoch);
    assert!(rl.is_active().is_none());

    // remaining=5 is below threshold — proactive wait activates
    rl.remaining = Some(5);
    assert!(rl.is_active().is_some());
}

#[test]
fn count_ci_runs_skips_the_reviews_own_checks() {
    let run = |name: &str, status: &str, conclusion: Option<&str>| types::GitHubCheckRun {
        id: 1,
        name: name.to_string(),
        status: status.to_string(),
        conclusion: conclusion.map(str::to_string),
        app: None,
    };
    let runs = vec![
        run("ci", "completed", Some("success")),
        run("review-gate", "completed", Some("failure")),
        run("orch/review", "in_progress", None),
    ];
    assert_eq!(GhHttp::count_ci_runs(&runs), (1, 1, 0, 0));

    let only_own = vec![run("review-gate", "completed", Some("failure"))];
    assert_eq!(GhHttp::count_ci_runs(&only_own), (0, 0, 0, 0));
    assert_eq!(
        GhHttp::combined_status_state(0, 0, 0, true, Some("clean")),
        "success"
    );
}

#[test]
fn combined_status_state_treats_empty_checks_as_pending_when_workflows_exist() {
    // No check runs yet and GitHub hasn't said it's clean → pending (wait for CI to queue)
    assert_eq!(
        GhHttp::combined_status_state(0, 0, 0, true, None),
        "pending"
    );
    // No workflows configured → no need to wait
    assert_eq!(
        GhHttp::combined_status_state(0, 0, 0, false, None),
        "success"
    );
    // Failing check → failure regardless
    assert_eq!(
        GhHttp::combined_status_state(3, 1, 0, true, None),
        "failure"
    );
    // paths-ignore filtered out all workflows: no check runs but GitHub says "clean"
    assert_eq!(
        GhHttp::combined_status_state(0, 0, 0, true, Some("clean")),
        "success"
    );
    // mergeable_state is dirty/blocked — still pending even with has_workflows
    assert_eq!(
        GhHttp::combined_status_state(0, 0, 0, true, Some("dirty")),
        "pending"
    );
}

#[test]
fn batch_is_pr_merged_by_branch_errors_when_repository_data_missing() {
    let resp = serde_json::json!({
        "data": {},
        "errors": [{ "message": "Something went wrong" }],
    });

    let err_text = GhHttp::parse_batch_pr_merged_response(&resp, &["branch".to_string()])
        .unwrap_err()
        .to_string();
    assert!(err_text.contains("missing /data/repository in GraphQL response"));
    assert!(err_text.contains("Something went wrong"));
}

#[test]
fn rate_limit_uses_reset_at_for_backoff() {
    let mut rl = make_rl(30, 900);
    // Set reset_at to 45s in the future
    let future_epoch = SystemTime::now()
        .duration_since(std_time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 45;
    rl.reset_at = Some(future_epoch);
    rl.record_rate_limit();
    // Backoff should use reset_at (~46s) not the base (30s)
    assert!(rl.backoff_delay.as_secs() >= 45);
    assert!(rl.backoff_delay.as_secs() <= 47);
}

#[tokio::test]
async fn send_with_retries_propagates_make_req_error() {
    let guard = env::var_os("GH_TOKEN");
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("GH_TOKEN", "test_token") };
    let gh = test_client();
    let result = gh
        .send_with_retries(
            || Err(anyhow::anyhow!("simulated request clone failure")),
            false,
        )
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert_eq!(err, "simulated request clone failure");
    match guard {
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        Some(v) => unsafe { env::set_var("GH_TOKEN", v) },
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        None => unsafe { env::remove_var("GH_TOKEN") },
    }
}

/// A transport-level failure (connection refused — GitHub never reached)
/// must exhaust retries without tripping the shared "github:5xx" circuit
/// breaker: that breaker is read by the routing-phase skip and other
/// subsystems as a signal specifically about GitHub's health, which a
/// local connectivity blip says nothing about.
#[tokio::test]
async fn send_with_retries_transport_error_does_not_trip_5xx_breaker() {
    let guard = env::var_os("GH_TOKEN");
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("GH_TOKEN", "test_token") };

    // Bind then immediately drop a listener to get a port nothing is
    // listening on, so connecting reliably fails with "connection refused"
    // rather than depending on an external unreachable host.
    let addr = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    };
    let url = format!("http://{addr}/");

    let gh = test_client();
    let make_req = || Ok(gh.client.get(&url));
    let result = gh.send_with_retries(make_req, false).await;

    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("is_connect=true") && err.to_lowercase().contains("refused"),
        "transport error should hold the source chain, got: {err}"
    );
    assert!(
        !cooldown::is_agent_in_cooldown("github:5xx"),
        "transport error must not trip the github:5xx circuit breaker"
    );
    assert!(cooldown::cooldown_until("github:5xx").is_none());

    match guard {
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        Some(v) => unsafe { env::set_var("GH_TOKEN", v) },
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        None => unsafe { env::remove_var("GH_TOKEN") },
    }
}

#[tokio::test]
async fn post_json_raw_propagates_body_read_errors_on_success() {
    let guard = env::var_os("GH_TOKEN");
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("GH_TOKEN", "test_token") };
    let server_url = spawn_truncated_body_server("{").await;
    let gh = test_client();

    let result = gh
        .post_json_raw(
            &format!("{server_url}/post"),
            &serde_json::json!({"ok": true}),
        )
        .await;

    let err = result.unwrap_err().to_string();
    assert!(
        !err.contains("EOF while parsing a value"),
        "should surface the body read error, got: {err}"
    );
    assert!(
        err.contains("error decoding response body")
            || err.contains("request or response body error")
            || err.contains("end of file before message length reached"),
        "expected a transport/body read error, got: {err}"
    );

    match guard {
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        Some(v) => unsafe { env::set_var("GH_TOKEN", v) },
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        None => unsafe { env::remove_var("GH_TOKEN") },
    }
}

#[tokio::test]
async fn patch_json_raw_propagates_body_read_errors_on_success() {
    let guard = env::var_os("GH_TOKEN");
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("GH_TOKEN", "test_token") };
    let server_url = spawn_truncated_body_server("{").await;
    let gh = test_client();

    let result = gh
        .patch_json_raw(
            &format!("{server_url}/patch"),
            &serde_json::json!({"ok": true}),
        )
        .await;

    let err = result.unwrap_err().to_string();
    assert!(
        !err.contains("EOF while parsing a value"),
        "should surface the body read error, got: {err}"
    );
    assert!(
        err.contains("error decoding response body")
            || err.contains("request or response body error")
            || err.contains("end of file before message length reached"),
        "expected a transport/body read error, got: {err}"
    );

    match guard {
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        Some(v) => unsafe { env::set_var("GH_TOKEN", v) },
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        None => unsafe { env::remove_var("GH_TOKEN") },
    }
}

#[tokio::test]
async fn graphql_request_propagates_body_read_errors_on_success() {
    let guard = env::var_os("GH_TOKEN");
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("GH_TOKEN", "test_token") };
    let server_url = spawn_truncated_body_server("{").await;
    let gh = test_client();
    let query = "query { viewer { login } }";

    let result = gh
        .graphql_request_to_url(
            &format!("{server_url}/graphql"),
            query,
            &[],
            Some(serde_json::json!({"k": "v"})),
        )
        .await;

    let err = result.unwrap_err().to_string();
    assert!(
        !err.contains("EOF while parsing a value"),
        "should surface the body read error, got: {err}"
    );
    assert!(
        err.contains("error decoding response body")
            || err.contains("request or response body error")
            || err.contains("end of file before message length reached"),
        "expected a transport/body read error, got: {err}"
    );

    match guard {
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        Some(v) => unsafe { env::set_var("GH_TOKEN", v) },
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        None => unsafe { env::remove_var("GH_TOKEN") },
    }
}

// ── get_all_pages integration tests (wiremock) ────────────────────────

#[cfg(test)]
mod pagination {
    use super::super::*;
    use crate::backends::BlockedByFetch;
    use crate::github::token::TokenResolver;
    use std::env;
    use std::ffi::OsString;
    use std::sync::Arc;
    use std::time;
    use std::time::SystemTime;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate, matchers};

    /// wiremock has no negated matcher, and the first poll must be matched on the
    /// absence of `If-None-Match` to tell it apart from the conditional one.
    struct NotMatcher<M>(M);

    impl<M: Match> Match for NotMatcher<M> {
        fn matches(&self, request: &Request) -> bool {
            !self.0.matches(request)
        }
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = env::var_os(key);
            // SAFETY: nextest runs each test in its own process, so no other test touches the environment
            unsafe { env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                // SAFETY: nextest runs each test in its own process, so no other test touches the environment
                Some(value) => unsafe { env::set_var(self.key, value) },
                // SAFETY: nextest runs each test in its own process, so no other test touches the environment
                None => unsafe { env::remove_var(self.key) },
            }
        }
    }

    /// Build a minimal GhHttp pointing at a wiremock server.
    /// The token resolver reads GH_TOKEN; we set it to a dummy value.
    fn make_client() -> (GhHttp, EnvVarGuard) {
        // Ensure a token is available so auth_header() doesn't bail.
        let guard = EnvVarGuard::set("GH_TOKEN", "test_token");
        let client = GhHttp {
            client: reqwest::Client::new(),
            token_resolver: Arc::new(TokenResolver::default_env()),
            app: None,
            owner_only: false,
            cached_username: OnceLock::new(),
            api_base: None,
        };
        (client, guard)
    }

    fn client_for(server: &MockServer) -> (GhHttp, EnvVarGuard) {
        let guard = EnvVarGuard::set("GH_TOKEN", "test_token");
        (GhHttp::for_test(server.uri(), None), guard)
    }

    fn check_run_json(
        id: u64,
        name: &str,
        status: &str,
        conclusion: Option<&str>,
    ) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "name": name,
            "status": status,
            "conclusion": conclusion,
        })
    }

    fn passing_check_runs(count: u64) -> Vec<serde_json::Value> {
        (1..=count)
            .map(|i| check_run_json(i, &format!("check-{i}"), "completed", Some("success")))
            .collect()
    }

    async fn mock_paginated_check_runs(
        server: &MockServer,
        sha: &str,
        page1: Vec<serde_json::Value>,
        page2: Vec<serde_json::Value>,
    ) {
        let total_count = page1.len() + page2.len();
        Mock::given(method("GET"))
            .and(path(format!("/repos/o/r/commits/{sha}/check-runs")))
            .and(query_param("filter", "latest"))
            .and(query_param("per_page", "100"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({
                        "total_count": total_count,
                        "check_runs": page1,
                    }))
                    .insert_header(
                        "Link",
                        format!("<{}/check-runs/page2>; rel=\"next\"", server.uri()),
                    ),
            )
            .expect(1)
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/check-runs/page2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total_count": total_count,
                "check_runs": page2,
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn get_all_pages_single_page() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/items"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([{"id": 1}, {"id": 2}])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let (gh, _guard) = make_client();
        let url = format!("{}/items", server.uri());
        let result: Vec<serde_json::Value> = gh.get_all_pages(&url, &[]).await.unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["id"], 1);
        assert_eq!(result[1]["id"], 2);
    }

    #[tokio::test]
    async fn get_all_pages_follows_link_next() {
        let server = MockServer::start().await;

        // Page 1 — includes Link header pointing to page 2
        Mock::given(method("GET"))
            .and(path("/items"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([{"id": 1}]))
                    .insert_header(
                        "Link",
                        format!("<{}/items/page2>; rel=\"next\"", server.uri()),
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;

        // Page 2 — no Link header (last page)
        Mock::given(method("GET"))
            .and(path("/items/page2"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([{"id": 2}, {"id": 3}])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let (gh, _guard) = make_client();
        let url = format!("{}/items", server.uri());
        let result: Vec<serde_json::Value> = gh.get_all_pages(&url, &[]).await.unwrap();

        assert_eq!(result.len(), 3);
        assert_eq!(result[0]["id"], 1);
        assert_eq!(result[1]["id"], 2);
        assert_eq!(result[2]["id"], 3);
    }

    #[tokio::test]
    async fn get_all_pages_stops_when_link_header_absent_on_subsequent_page() {
        // Simulates a proxy/transient response that drops the Link header mid-pagination.
        // The previous `.unwrap()` would panic here; the fix breaks the loop instead.
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/items"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([{"id": 1}]))
                    .insert_header(
                        "Link",
                        format!("<{}/items/page2>; rel=\"next\"", server.uri()),
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;

        // Page 2 response has NO Link header — pagination should stop cleanly.
        Mock::given(method("GET"))
            .and(path("/items/page2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([{"id": 2}])))
            .expect(1)
            .mount(&server)
            .await;

        let (gh, _guard) = make_client();
        let url = format!("{}/items", server.uri());
        let result: Vec<serde_json::Value> = gh.get_all_pages(&url, &[]).await.unwrap();

        // Must not panic and must return both items collected so far.
        assert_eq!(result.len(), 2);
    }

    #[tokio::test]
    async fn get_all_pages_stops_on_malformed_link_header() {
        // A malformed Link header (no angle-bracket URL) yields None from parse_link_next,
        // which should stop pagination rather than panic.
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/items"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([{"id": 1}]))
                    .insert_header("Link", "garbage; rel=\"next\""),
            )
            .expect(1)
            .mount(&server)
            .await;

        let (gh, _guard) = make_client();
        let url = format!("{}/items", server.uri());
        let result: Vec<serde_json::Value> = gh.get_all_pages(&url, &[]).await.unwrap();

        // Malformed Link header → pagination stops after first page, no panic.
        assert_eq!(result.len(), 1);
    }

    #[tokio::test]
    async fn get_all_pages_returns_error_on_malformed_json_body() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/items"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not valid json"))
            .expect(3)
            .mount(&server)
            .await;

        let (gh, _guard) = make_client();
        let url = format!("{}/items", server.uri());
        let result = gh.get_all_pages::<serde_json::Value>(&url, &[]).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("JSON decode failed") || err.contains("expected ident"),
            "error should mention JSON decode or serde error: {err}"
        );
    }

    // ── conditional request / rate-limit tests ───────────────────────────

    /// Restores the global REST limiter and the page cache, both of which these two
    /// tests have to poison to exercise the real request path.
    struct GlobalStateGuard;

    impl Drop for GlobalStateGuard {
        fn drop(&mut self) {
            if let Ok(mut rl) = REST_RATE_LIMIT.lock() {
                *rl = RateLimit::new();
            }
            if let Ok(mut cache) = PAGE_CACHE.lock() {
                cache.clear();
            }
        }
    }

    #[tokio::test]
    async fn get_all_pages_replays_body_on_304() {
        let _reset = GlobalStateGuard;
        let server = MockServer::start().await;

        // First poll: 200 with an ETag.
        Mock::given(method("GET"))
            .and(path("/issues"))
            .and(matchers::header_exists("authorization"))
            .and(NotMatcher(matchers::header_exists("if-none-match")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([{"number": 1}]))
                    .insert_header("ETag", "\"abc123\""),
            )
            .expect(1)
            .mount(&server)
            .await;

        // Second poll: orch sends If-None-Match, GitHub answers 304 with no body.
        Mock::given(method("GET"))
            .and(path("/issues"))
            .and(matchers::header("if-none-match", "\"abc123\""))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;

        let (gh, _guard) = make_client();
        let url = format!("{}/issues", server.uri());

        let first: Vec<serde_json::Value> = gh.get_all_pages(&url, &[]).await.unwrap();
        let second: Vec<serde_json::Value> = gh.get_all_pages(&url, &[]).await.unwrap();

        assert_eq!(first, second);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0]["number"], 1);
    }

    #[tokio::test]
    async fn list_blocked_by_replays_body_on_304() {
        let _reset = GlobalStateGuard;
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues/1/dependencies/blocked_by"))
            .and(matchers::header_exists("authorization"))
            .and(NotMatcher(matchers::header_exists("if-none-match")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([{
                        "id": 99,
                        "number": 7,
                        "state": "open",
                        "repository": { "full_name": "owner/repo" }
                    }]))
                    .insert_header("ETag", "\"dep123\""),
            )
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues/1/dependencies/blocked_by"))
            .and(matchers::header("if-none-match", "\"dep123\""))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;

        let (gh, _guard) = client_for(&server);
        let first = gh.list_blocked_by("owner/repo", 1).await.unwrap();
        let second = gh.list_blocked_by("owner/repo", 1).await.unwrap();

        assert_eq!(first, second);
        match second {
            BlockedByFetch::Native(items) => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].github_id, 99);
                assert_eq!(items[0].number, 7);
                assert_eq!(items[0].repo, "owner/repo");
                assert_eq!(items[0].state, "open");
            }
            other => panic!("expected Native, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_blocked_by_does_not_cache_an_unparseable_body() {
        let _reset = GlobalStateGuard;
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues/1/dependencies/blocked_by"))
            .and(NotMatcher(matchers::header_exists("if-none-match")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("not json")
                    .insert_header("ETag", "\"bad1\""),
            )
            .expect(2)
            .mount(&server)
            .await;

        let (gh, _guard) = client_for(&server);
        let first = gh.list_blocked_by("owner/repo", 1).await.unwrap();
        let second = gh.list_blocked_by("owner/repo", 1).await.unwrap();

        assert!(matches!(first, BlockedByFetch::Failed(_)));
        assert!(matches!(second, BlockedByFetch::Failed(_)));
    }

    #[tokio::test]
    async fn list_blocked_by_abuse_detection_403_fails() {
        let _reset = GlobalStateGuard;
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues/1/dependencies/blocked_by"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_string("You have triggered an abuse detection mechanism"),
            )
            .mount(&server)
            .await;

        let (gh, _guard) = client_for(&server);
        let result = gh.list_blocked_by("owner/repo", 1).await.unwrap();

        assert!(matches!(result, BlockedByFetch::Failed(_)), "{result:?}");
    }

    #[tokio::test]
    async fn exhausted_quota_skips_further_calls_until_reset() {
        let _reset = GlobalStateGuard;
        let server = MockServer::start().await;

        let reset_at = SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 600;

        // One response, and only one: it reports an exhausted core quota, so every
        // later call must be skipped locally rather than sent.
        Mock::given(method("GET"))
            .and(path("/issues"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([{"number": 1}]))
                    .insert_header("x-ratelimit-remaining", "0")
                    .insert_header("x-ratelimit-reset", reset_at.to_string().as_str()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let (gh, _guard) = make_client();
        let url = format!("{}/issues", server.uri());

        let first: Vec<serde_json::Value> = gh.get_all_pages(&url, &[]).await.unwrap();
        assert_eq!(first.len(), 1);

        // The next call returns immediately instead of waiting out the 600s window.
        let start = Instant::now();
        let err = gh
            .get_all_pages::<serde_json::Value>(&url, &[("state", "open")])
            .await
            .unwrap_err();
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "call blocked for {:?} instead of failing fast",
            start.elapsed()
        );
        assert!(err.to_string().contains("rate-limited"), "{err}");

        // The engine reports the window from the same state the skip reads.
        let m = rate_limit_metrics();
        assert_eq!(m.rest_remaining, Some(0));
        assert_eq!(m.rest_reset_at, Some(reset_at));
        assert!(GhHttp::is_rate_limited().is_some());

        // `.expect(1)` on the mock is verified on drop: no second request went out.
    }

    // ── get_all_pages_from_wrapper tests ─────────────────────────────────

    #[tokio::test]
    async fn get_all_pages_from_wrapper_single_page() {
        // Simulates GitHub's check-runs response: a wrapper object with the
        // array nested under a named field.
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/check-runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total_count": 2,
                "check_runs": [{"id": 1, "name": "build"}, {"id": 2, "name": "test"}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (gh, _guard) = make_client();
        let url = format!("{}/check-runs", server.uri());
        let result: Vec<serde_json::Value> = gh
            .get_all_pages_from_wrapper(&url, "check_runs")
            .await
            .unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["id"], 1);
        assert_eq!(result[1]["id"], 2);
    }

    #[tokio::test]
    async fn get_all_pages_from_wrapper_follows_link_next() {
        // Two-page check-runs response (common for repos with >100 distinct check names).
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/check-runs"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({
                        "total_count": 3,
                        "check_runs": [{"id": 1, "name": "build"}]
                    }))
                    .insert_header(
                        "Link",
                        format!("<{}/check-runs/page2>; rel=\"next\"", server.uri()),
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/check-runs/page2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total_count": 3,
                "check_runs": [{"id": 2, "name": "lint"}, {"id": 3, "name": "test"}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (gh, _guard) = make_client();
        let url = format!("{}/check-runs", server.uri());
        let result: Vec<serde_json::Value> = gh
            .get_all_pages_from_wrapper(&url, "check_runs")
            .await
            .unwrap();

        assert_eq!(result.len(), 3);
        assert_eq!(result[0]["id"], 1);
        assert_eq!(result[1]["id"], 2);
        assert_eq!(result[2]["id"], 3);
    }

    #[tokio::test]
    async fn get_all_pages_from_wrapper_empty_field_returns_empty_vec() {
        // When the array field is missing or empty, the function should return
        // an empty vec rather than erroring.
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/check-runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total_count": 0,
                "check_runs": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (gh, _guard) = make_client();
        let url = format!("{}/check-runs", server.uri());
        let result: Vec<serde_json::Value> = gh
            .get_all_pages_from_wrapper(&url, "check_runs")
            .await
            .unwrap();

        assert_eq!(result.len(), 0);
    }

    #[tokio::test]
    async fn get_all_pages_from_wrapper_error_on_non_success_status() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/check-runs"))
            .respond_with(ResponseTemplate::new(422).set_body_string("Unprocessable Entity"))
            .expect(1)
            .mount(&server)
            .await;

        let (gh, _guard) = make_client();
        let url = format!("{}/check-runs", server.uri());
        let result = gh
            .get_all_pages_from_wrapper::<serde_json::Value>(&url, "check_runs")
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("422") || err.contains("failed"),
            "error should indicate failure: {err}"
        );
    }

    #[tokio::test]
    async fn get_combined_status_counts_a_failure_on_a_later_page() {
        let server = MockServer::start().await;
        let sha = "abc123";
        mock_paginated_check_runs(
            &server,
            sha,
            passing_check_runs(30),
            vec![check_run_json(31, "check-31", "completed", Some("failure"))],
        )
        .await;

        let (gh, _guard) = client_for(&server);
        let (state, total, passing, failing, pending) = gh
            .get_combined_status("o/r", sha, true, None)
            .await
            .unwrap();

        assert_eq!(state, "failure");
        assert_eq!(total, 31);
        assert_eq!(passing, 30);
        assert_eq!(failing, 1);
        assert_eq!(pending, 0);
    }

    #[tokio::test]
    async fn get_combined_status_counts_an_in_progress_run_on_a_later_page() {
        let server = MockServer::start().await;
        let sha = "abc123";
        mock_paginated_check_runs(
            &server,
            sha,
            passing_check_runs(30),
            vec![check_run_json(31, "check-31", "in_progress", None)],
        )
        .await;

        let (gh, _guard) = client_for(&server);
        let (state, total, passing, failing, pending) = gh
            .get_combined_status("o/r", sha, true, None)
            .await
            .unwrap();

        assert_eq!(state, "pending");
        assert_eq!(total, 31);
        assert_eq!(passing, 30);
        assert_eq!(failing, 0);
        assert_eq!(pending, 1);
    }
}

// ── 401 token-refresh tests (wiremock) ────────────────────────────────

#[cfg(test)]
mod auth_retry {
    use super::test_client;
    use crate::github::token;
    use std::env;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn bad_credentials() -> ResponseTemplate {
        ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "message": "Bad credentials",
            "status": 401
        }))
    }

    #[expect(
        clippy::await_holding_lock,
        reason = "the env lock stays held across the awaits so tests do not race on GH_TOKEN"
    )]
    #[tokio::test]
    async fn retries_once_with_fresh_token_after_401() {
        let _env_guard = token::ENV_LOCK.lock().unwrap();
        let prev = env::var_os("GH_TOKEN");
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        unsafe { env::set_var("GH_TOKEN", "bad_token") };

        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_in_handler = Arc::clone(&hits);
        Mock::given(method("GET"))
            .respond_with(move |_req: &Request| {
                if hits_in_handler.fetch_add(1, Ordering::SeqCst) == 0 {
                    // The 401 handler clears the cache and re-resolves; simulate the
                    // operator having fixed the env var in the meantime
                    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
                    unsafe { env::set_var("GH_TOKEN", "good_token") };
                    bad_credentials()
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true }))
                }
            })
            .mount(&server)
            .await;

        let gh = test_client();
        let result: anyhow::Result<serde_json::Value> =
            gh.get_json(&format!("{}/x", server.uri())).await;

        match prev {
            // SAFETY: nextest runs each test in its own process, so no other test touches the environment
            Some(v) => unsafe { env::set_var("GH_TOKEN", v) },
            // SAFETY: nextest runs each test in its own process, so no other test touches the environment
            None => unsafe { env::remove_var("GH_TOKEN") },
        }

        assert!(
            result.is_ok(),
            "retry with re-resolved token should succeed: {:?}",
            result.err()
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2, "exactly one retry expected");
    }

    #[expect(
        clippy::await_holding_lock,
        reason = "the env lock stays held across the awaits so tests do not race on GH_TOKEN"
    )]
    #[tokio::test]
    async fn double_401_bails_with_distinct_auth_error() {
        let _env_guard = token::ENV_LOCK.lock().unwrap();
        let prev = env::var_os("GH_TOKEN");
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        unsafe { env::set_var("GH_TOKEN", "bad_token") };

        let server = MockServer::start().await;
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_in_handler = Arc::clone(&hits);
        Mock::given(method("GET"))
            .respond_with(move |_req: &Request| {
                hits_in_handler.fetch_add(1, Ordering::SeqCst);
                bad_credentials()
            })
            .mount(&server)
            .await;

        let gh = test_client();
        let result: anyhow::Result<serde_json::Value> =
            gh.get_json(&format!("{}/x", server.uri())).await;

        match prev {
            // SAFETY: nextest runs each test in its own process, so no other test touches the environment
            Some(v) => unsafe { env::set_var("GH_TOKEN", v) },
            // SAFETY: nextest runs each test in its own process, so no other test touches the environment
            None => unsafe { env::remove_var("GH_TOKEN") },
        }

        let err = result.expect_err("persisting 401 must fail").to_string();
        assert!(
            err.contains("token re-resolved after cache clear but still rejected"),
            "expected distinct auth-failure error, got: {err}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "must retry exactly once, not loop"
        );
    }
}

#[cfg(test)]
mod contents_and_rulesets {
    use super::super::*;
    use crate::github::types::RequiredStatusCheck;
    use std::env;
    use std::ffi::OsString;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = env::var_os(key);
            // SAFETY: nextest runs each test in its own process, so no other test touches the environment
            unsafe { env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                // SAFETY: nextest runs each test in its own process, so no other test touches the environment
                Some(value) => unsafe { env::set_var(self.key, value) },
                // SAFETY: nextest runs each test in its own process, so no other test touches the environment
                None => unsafe { env::remove_var(self.key) },
            }
        }
    }

    fn client(server: &MockServer) -> (GhHttp, EnvVarGuard) {
        let guard = EnvVarGuard::set("GH_TOKEN", "test_token");
        (GhHttp::for_test(server.uri(), None), guard)
    }

    #[tokio::test]
    async fn has_workflow_on_branch_is_false_when_the_directory_is_missing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/contents/.github/workflows"))
            .and(query_param("ref", "main"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;
        let (gh, _guard) = client(&server);
        assert!(!gh.has_workflow_on_branch("o/r", "main").await.unwrap());
    }

    #[tokio::test]
    async fn has_workflow_on_branch_is_true_when_a_yaml_file_exists() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/contents/.github/workflows"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "name": "ci.yml", "type": "file" }
            ])))
            .mount(&server)
            .await;
        let (gh, _guard) = client(&server);
        assert!(gh.has_workflow_on_branch("o/r", "main").await.unwrap());
    }

    #[tokio::test]
    async fn has_file_on_branch_is_false_on_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/contents/shipcheck.toml"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;
        let (gh, _guard) = client(&server);
        assert!(
            !gh.has_file_on_branch("o/r", "shipcheck.toml", "main")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn create_required_check_ruleset_posts_the_payload() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/rulesets"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": 1,
                "name": REQUIRED_CHECK_RULESET_NAME
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (gh, _guard) = client(&server);
        gh.create_required_check_ruleset(
            "o/r",
            &[RequiredStatusCheck {
                context: "test".to_string(),
                integration_id: Some(15368),
            }],
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn get_named_ruleset_required_checks_reads_the_named_ruleset() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/rulesets"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "id": 8, "name": "other" },
                { "id": 9, "name": REQUIRED_CHECK_RULESET_NAME }
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/rulesets/9"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 9,
                "name": REQUIRED_CHECK_RULESET_NAME,
                "rules": [{
                    "type": "required_status_checks",
                    "parameters": {
                        "required_status_checks": [
                            { "context": "test", "integration_id": 15368 }
                        ]
                    }
                }]
            })))
            .mount(&server)
            .await;
        let (gh, _guard) = client(&server);
        let (id, checks) = gh
            .get_named_ruleset_required_checks("o/r", REQUIRED_CHECK_RULESET_NAME)
            .await
            .unwrap()
            .expect("ruleset");
        assert_eq!(id, 9);
        assert_eq!(
            checks,
            vec![RequiredStatusCheck {
                context: "test".to_string(),
                integration_id: Some(15368),
            }]
        );
    }

    #[tokio::test]
    async fn get_named_ruleset_required_checks_is_none_when_missing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/rulesets"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "id": 8, "name": "other" }
            ])))
            .mount(&server)
            .await;
        let (gh, _guard) = client(&server);
        assert!(
            gh.get_named_ruleset_required_checks("o/r", REQUIRED_CHECK_RULESET_NAME)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn update_required_check_ruleset_puts_the_payload() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/repos/o/r/rulesets/9"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 9,
                "name": REQUIRED_CHECK_RULESET_NAME
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (gh, _guard) = client(&server);
        gh.update_required_check_ruleset(
            "o/r",
            9,
            &[RequiredStatusCheck {
                context: "test".to_string(),
                integration_id: Some(15368),
            }],
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn get_branch_sha_reads_commit_sha() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/branches/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "main",
                "commit": { "sha": "abc123" }
            })))
            .mount(&server)
            .await;
        let (gh, _guard) = client(&server);
        assert_eq!(gh.get_branch_sha("o/r", "main").await.unwrap(), "abc123");
    }
}
