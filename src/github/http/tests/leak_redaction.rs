use super::GhHttp;
use super::test_client;
use crate::github::token;
use std::env;
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const TOKEN: &str = "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";

fn leaky_body() -> String {
    format!("agent notes mentioning {TOKEN}")
}

fn client_for(server: &MockServer) -> GhHttp {
    let mut gh = test_client();
    gh.api_base = Some(server.uri());
    gh
}

fn posted_json_body(raw: &str) -> String {
    let payload: serde_json::Value = serde_json::from_str(raw).expect("request body is JSON");
    payload["body"]
        .as_str()
        .expect("payload has a string body")
        .to_string()
}

fn assert_token_redacted(raw: &str) {
    assert!(
        !raw.contains(TOKEN),
        "planted token must not appear in the request payload, got: {raw}"
    );
    let body = posted_json_body(raw);
    assert!(
        body.contains("[REDACTED:github_token]"),
        "redacted marker missing from posted body: {body}"
    );
    assert!(
        body.contains("potential secret(s) detected and redacted"),
        "security notice missing from posted body: {body}"
    );
}

#[expect(
    clippy::await_holding_lock,
    reason = "the env lock stays held across the awaits so tests do not race on GH_TOKEN"
)]
#[tokio::test]
async fn add_comment_redacts_github_token() {
    let _env_guard = token::ENV_LOCK.lock().unwrap();
    let prev = env::var_os("GH_TOKEN");
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("GH_TOKEN", "test_token") };

    let server = MockServer::start().await;
    let captured = Arc::new(Mutex::new(None::<String>));
    let captured_in_handler = Arc::clone(&captured);
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/issues/1/comments"))
        .respond_with(move |req: &Request| {
            *captured_in_handler.lock().unwrap() =
                Some(String::from_utf8_lossy(&req.body).into_owned());
            ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let gh = client_for(&server);
    gh.add_comment("owner/repo", "1", &leaky_body())
        .await
        .expect("add_comment should succeed");

    match prev {
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        Some(v) => unsafe { env::set_var("GH_TOKEN", v) },
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        None => unsafe { env::remove_var("GH_TOKEN") },
    }

    let raw = captured
        .lock()
        .unwrap()
        .clone()
        .expect("add_comment must send a request body");
    assert_token_redacted(&raw);
}

#[expect(
    clippy::await_holding_lock,
    reason = "the env lock stays held across the awaits so tests do not race on GH_TOKEN"
)]
#[tokio::test]
async fn create_pr_redacts_github_token() {
    let _env_guard = token::ENV_LOCK.lock().unwrap();
    let prev = env::var_os("GH_TOKEN");
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("GH_TOKEN", "test_token") };

    let server = MockServer::start().await;
    let captured = Arc::new(Mutex::new(None::<String>));
    let captured_in_handler = Arc::clone(&captured);
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/pulls"))
        .respond_with(move |req: &Request| {
            *captured_in_handler.lock().unwrap() =
                Some(String::from_utf8_lossy(&req.body).into_owned());
            ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "html_url": "https://github.com/owner/repo/pull/1"
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let gh = client_for(&server);
    gh.create_pr("owner/repo", "title", &leaky_body(), "head", "main")
        .await
        .expect("create_pr should succeed");

    match prev {
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        Some(v) => unsafe { env::set_var("GH_TOKEN", v) },
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        None => unsafe { env::remove_var("GH_TOKEN") },
    }

    let raw = captured
        .lock()
        .unwrap()
        .clone()
        .expect("create_pr must send a request body");
    assert_token_redacted(&raw);
}

#[expect(
    clippy::await_holding_lock,
    reason = "the env lock stays held across the awaits so tests do not race on GH_TOKEN"
)]
#[tokio::test]
async fn update_pr_body_redacts_github_token() {
    let _env_guard = token::ENV_LOCK.lock().unwrap();
    let prev = env::var_os("GH_TOKEN");
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("GH_TOKEN", "test_token") };

    let server = MockServer::start().await;
    let captured = Arc::new(Mutex::new(None::<String>));
    let captured_in_handler = Arc::clone(&captured);
    Mock::given(method("PATCH"))
        .and(path("/repos/owner/repo/pulls/7"))
        .respond_with(move |req: &Request| {
            *captured_in_handler.lock().unwrap() =
                Some(String::from_utf8_lossy(&req.body).into_owned());
            ResponseTemplate::new(200).set_body_json(serde_json::json!({}))
        })
        .expect(1)
        .mount(&server)
        .await;

    let gh = client_for(&server);
    gh.update_pr_body("owner/repo", 7, &leaky_body())
        .await
        .expect("update_pr_body should succeed");

    match prev {
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        Some(v) => unsafe { env::set_var("GH_TOKEN", v) },
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        None => unsafe { env::remove_var("GH_TOKEN") },
    }

    let raw = captured
        .lock()
        .unwrap()
        .clone()
        .expect("update_pr_body must send a request body");
    assert_token_redacted(&raw);
}
