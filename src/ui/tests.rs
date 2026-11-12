use std::fs;
use std::os::unix::fs::symlink;

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

use super::*;

const TOKEN: &str = "secret-token";
const HOST: &str = "127.0.0.1:43917";

async fn app() -> Router {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let (events, _) = broadcast::channel(4);
    router(App {
        store,
        events,
        token: Arc::from(TOKEN),
        host: Arc::from(HOST),
    })
}

async fn get(path: &str, host: &str, cookie: Option<&str>) -> Response {
    let mut req = Request::get(path).header(header::HOST, host);
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }

    app()
        .await
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn an_api_call_without_the_cookie_is_refused() {
    let res = get("/api/tasks?repo=o/r", HOST, None).await;
    assert_eq!(
        res.status(),
        StatusCode::UNAUTHORIZED,
        "no cookie must not pass"
    );
}

#[tokio::test]
async fn a_wrong_cookie_is_refused() {
    let res = get("/api/tasks?repo=o/r", HOST, Some("orch_ui=nope")).await;
    assert_eq!(
        res.status(),
        StatusCode::UNAUTHORIZED,
        "a wrong token must not pass"
    );
}

#[tokio::test]
async fn a_rebound_host_is_refused_even_with_the_cookie() {
    let res = get(
        "/api/tasks?repo=o/r",
        "evil.example:43917",
        Some("orch_ui=secret-token"),
    )
    .await;
    assert_eq!(
        res.status(),
        StatusCode::UNAUTHORIZED,
        "a DNS rebinding page sends its own Host"
    );
}

#[tokio::test]
async fn the_right_cookie_and_host_read_the_board() {
    let res = get(
        "/api/tasks?repo=o/r",
        HOST,
        Some("other=1; orch_ui=secret-token"),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "the owner's cookie must pass");
}

#[tokio::test]
async fn the_token_in_the_url_becomes_a_strict_cookie() {
    let res = get("/?token=secret-token", HOST, None).await;
    assert_eq!(
        res.status(),
        StatusCode::SEE_OTHER,
        "the token is traded for a cookie"
    );
    let cookie = res
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(cookie.starts_with("orch_ui=secret-token"), "{cookie}");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
}

#[tokio::test]
async fn a_wrong_token_in_the_url_sets_no_cookie() {
    let res = get("/?token=nope", HOST, None).await;
    assert_eq!(
        res.status(),
        StatusCode::UNAUTHORIZED,
        "a wrong token is refused"
    );
    assert!(
        res.headers().get(header::SET_COOKIE).is_none(),
        "no cookie for a wrong token"
    );
}

#[tokio::test]
async fn the_page_carries_a_strict_content_security_policy() {
    let res = get("/", HOST, Some("orch_ui=secret-token")).await;
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "the page loads with the cookie"
    );
    let csp = res
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(csp.contains("default-src 'none'"), "{csp}");
}

#[test]
fn config_that_is_not_yaml_is_refused() {
    let problem = config_problem("", "projects: [unclosed");
    assert!(
        problem.is_some_and(|p| p.contains("not valid YAML")),
        "broken YAML must not replace the config"
    );
}

#[test]
fn a_project_config_must_keep_its_repo() {
    assert!(
        config_problem("o/r", "gh:\n  repo: o/other\n").is_some(),
        "a changed gh.repo would drop the project"
    );
    assert!(
        config_problem("o/r", "workflow: {}\n").is_some(),
        "a missing gh.repo would drop the project"
    );
    assert!(
        config_problem("o/r", "gh:\n  repo: o/r\n# a comment\n").is_none(),
        "a valid project config passes"
    );
}

#[test]
fn the_global_config_only_needs_to_parse() {
    assert!(
        config_problem("", "projects:\n  - ~/code/app\n").is_none(),
        "valid YAML passes"
    );
}

#[test]
fn the_version_follows_the_text() {
    assert_eq!(version_of("a"), version_of("a"), "same text, same version");
    assert_ne!(
        version_of("a"),
        version_of("b"),
        "an edit changes the version"
    );
}

async fn task_through(store: &TaskStore, path: &[TaskStatus]) -> i64 {
    let id = store
        .create_internal("o/r", "a task", "", "test", "ui", None)
        .await
        .unwrap();
    for status in path {
        store.update_status(id, *status).await.unwrap();
    }
    id
}

#[tokio::test]
async fn a_merged_task_shows_in_the_day_and_the_stats() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = task_through(
        &store,
        &[TaskStatus::Routed, TaskStatus::InProgress, TaskStatus::Done],
    )
    .await;
    let done = store
        .entered_between(
            TaskStatus::Done,
            "2000-01-01T00:00:00Z",
            "2999-01-01T00:00:00Z",
            None,
        )
        .await
        .unwrap();
    assert_eq!(done.len(), 1, "the merge is in the range");
    assert_eq!(done[0].1.id, id, "it is the task that merged");

    let stats = store.stats_since("2000-01-01T00:00:00Z").await.unwrap();
    assert_eq!(stats.merged.len(), 1, "the stats count the merge once");

    let since = store.entered_status(&[id]).await.unwrap();
    assert!(
        since.contains_key(&id),
        "a done task knows when it got there"
    );
}

#[tokio::test]
async fn a_range_outside_the_merge_is_empty() {
    let store = TaskStore::open_memory().await.unwrap();
    task_through(&store, &[TaskStatus::Done]).await;
    let done = store
        .entered_between(
            TaskStatus::Done,
            "2000-01-01T00:00:00Z",
            "2000-01-02T00:00:00Z",
            None,
        )
        .await
        .unwrap();
    assert!(done.is_empty(), "nothing merged in 2000");
}

#[tokio::test]
async fn the_inbox_holds_blocked_and_human_tasks_only() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    task_through(&store, &[TaskStatus::Blocked]).await;
    task_through(&store, &[TaskStatus::Routed]).await;
    let open = store.list_open(None).await.unwrap();
    let waiting = cards_waiting(&store, open).await;
    assert_eq!(waiting.len(), 1, "only the blocked task waits on a person");
    assert_eq!(waiting[0].status, "blocked", "and it is the blocked one");
}

#[test]
fn a_local_day_becomes_store_instants() {
    assert_eq!(
        store_instant("2026-11-11T03:00:00.000Z").as_deref(),
        Some("2026-11-11T03:00:00Z"),
        "milliseconds drop so text comparison orders it"
    );
    assert_eq!(
        store_instant("2026-11-11T00:00:00-03:00").as_deref(),
        Some("2026-11-11T03:00:00Z"),
        "an offset becomes UTC"
    );
    assert!(
        store_instant("yesterday").is_none(),
        "anything else is refused"
    );
}

async fn get_with(path: &str, extra: &[(&str, &str)]) -> Response {
    let mut req = Request::get(path)
        .header(header::HOST, HOST)
        .header(header::COOKIE, "orch_ui=secret-token");
    for (k, v) in extra {
        req = req.header(*k, *v);
    }

    app()
        .await
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_page_on_another_local_port_is_refused() {
    let res = get_with("/api/tasks?repo=o/r", &[("sec-fetch-site", "same-site")]).await;
    assert_eq!(
        res.status(),
        StatusCode::UNAUTHORIZED,
        "another port of 127.0.0.1"
    );
    let res = get_with(
        "/api/tasks?repo=o/r",
        &[("origin", "http://127.0.0.1:3000")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "a foreign origin");
}

#[tokio::test]
async fn the_ui_page_itself_passes() {
    let res = get_with(
        "/api/tasks?repo=o/r",
        &[
            ("sec-fetch-site", "same-origin"),
            ("origin", "http://127.0.0.1:43917"),
        ],
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK, "its own page");
}

#[tokio::test]
async fn every_api_route_refuses_a_request_without_the_cookie() {
    let routes = [
        "/app.js",
        "/app.css",
        "/api/projects",
        "/api/tasks?repo=o/r",
        "/api/running",
        "/api/inbox",
        "/api/day?from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z",
        "/api/task?id=1",
        "/api/pr?id=1",
        "/api/usage",
        "/api/stats?days=7",
        "/api/config",
        "/api/chat?repo=o/r",
        "/api/plan?repo=o/r",
        "/api/agents",
        "/api/setup",
        "/api/setup/login",
        "/api/setup/repos",
        "/api/setup/owners",
        "/api/setup/dirs",
        "/api/events?repo=o/r",
        "/api/output?id=1",
        "/api/logs",
        "/api/activity",
    ];
    for route in routes {
        let res = get(route, HOST, None).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{route}");
    }
}

#[test]
fn saving_a_symlinked_config_keeps_the_link() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("dotfiles-config.yml");
    let link = dir.path().join("config.yml");
    fs::write(&real, "a: 1\n").unwrap();
    symlink(&real, &link).unwrap();
    let backups = tempfile::tempdir().unwrap();
    save_config(&link, "a: 1\n", "a: 2\n", backups.path()).unwrap();
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link stays"
    );
    assert_eq!(
        fs::read_to_string(&real).unwrap(),
        "a: 2\n",
        "the target has the edit"
    );
    assert!(
        !dir.path().join("config.yml.bak").exists(),
        "no backup beside the file"
    );
    assert_eq!(
        fs::read_dir(backups.path()).unwrap().count(),
        1,
        "the old text is kept in the state dir"
    );
}
