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
