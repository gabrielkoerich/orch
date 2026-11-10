use super::GhHttp;
use crate::github::token;
use std::env;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn closed_pr(number: u64, branch: &str, merged_at: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "state": "closed",
        "merged_at": merged_at,
        "head": { "ref": branch }
    })
}

fn client(server: &MockServer) -> GhHttp {
    GhHttp::for_test(server.uri(), None)
}

#[expect(
    clippy::await_holding_lock,
    reason = "the env lock stays held across the awaits so tests do not race on GH_TOKEN"
)]
#[tokio::test]
async fn is_pr_merged_skips_a_merged_pr_for_a_different_branch() {
    let _env_guard = token::ENV_LOCK.lock().unwrap();
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("GH_TOKEN", "test_token") };

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls"))
        .and(query_param("state", "closed"))
        .and(query_param("head", "owner:task-branch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            closed_pr(11, "other-branch", Some("2026-01-01T00:00:00Z")),
            closed_pr(12, "task-branch", None)
        ])))
        .mount(&server)
        .await;

    let gh = client(&server);
    assert!(!gh.is_pr_merged("owner/repo", "task-branch").await.unwrap());
}

#[expect(
    clippy::await_holding_lock,
    reason = "the env lock stays held across the awaits so tests do not race on GH_TOKEN"
)]
#[tokio::test]
async fn is_pr_merged_finds_a_matching_merged_pr_after_a_mismatched_head() {
    let _env_guard = token::ENV_LOCK.lock().unwrap();
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("GH_TOKEN", "test_token") };

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls"))
        .and(query_param("state", "closed"))
        .and(query_param("head", "owner:task-branch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            closed_pr(11, "other-branch", Some("2026-01-01T00:00:00Z")),
            closed_pr(12, "task-branch", Some("2026-01-02T00:00:00Z"))
        ])))
        .mount(&server)
        .await;

    let gh = client(&server);
    assert!(gh.is_pr_merged("owner/repo", "task-branch").await.unwrap());
}
