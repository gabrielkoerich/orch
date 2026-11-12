//! The web UI the daemon serves on 127.0.0.1: projects, a board of their tasks, live agent output,
//! the service log and a chat per project

mod chat;
mod compose;
mod plan;
mod setup;

use std::collections::HashMap;
use std::convert::Infallible;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::Stream;
use futures::stream;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tokio::fs as tokio_fs;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::task;
use tokio::time;
use tokio::time::Instant;

use crate::cli::ndjson::Formatter;
use crate::cli::stream::{Tail, newest_output};
use crate::cli::task_ref;
use crate::cluster::local::write_private;
use crate::config;
use crate::engine::cooldown;
use crate::engine::events::TaskEvent;
use crate::github::http::GhHttp;
use crate::home;
use crate::plan::{HUMAN_LABEL, NO_AGENT_LABEL};
use crate::security;
use crate::store::{StatsEvents, StatusChange, Task, TaskRun, TaskStatus, TaskStore};

/// Fixed so a bookmark keeps working, and below the ephemeral range so no outgoing socket takes it
pub const PORT: u16 = 43917;
const COOKIE: &str = "orch_ui";
const LOGIN_CODE_TTL: Duration = Duration::from_secs(60);
const DONE_ON_BOARD: i64 = 20;
const ERROR_CHARS: usize = 400;
const TAIL_POLL: Duration = Duration::from_secs(1);

const INDEX_HTML: &str = include_str!("../../web/index.html");
const APP_CSS: &str = include_str!("../../web/app.css");
// Built by Bun into web/dist on release, build.rs points at it or at a page saying how to build it
const APP_JS: &str = include_str!(env!("ORCH_WEB_JS"));
// The page loads only its own script and style, so injected markup cannot reach another host
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

#[derive(Clone)]
struct App {
    store: Arc<TaskStore>,
    events: broadcast::Sender<TaskEvent>,
    token: Arc<str>,
    host: Arc<str>,
    /// One-time sign-in codes and when each stops working
    codes: Arc<Mutex<HashMap<String, Instant>>>,
}

/// Where `orch ui` finds the running UI
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub port: u16,
    pub token: String,
}

#[derive(Deserialize)]
pub(super) struct RepoQuery {
    repo: String,
}

#[derive(Deserialize)]
struct DayQuery {
    from: String,
    to: String,
    #[serde(default)]
    repo: String,
}

#[derive(Serialize)]
struct Inbox {
    blocked: Vec<Card>,
    human: Vec<Card>,
}

#[derive(Serialize)]
struct Landed {
    at: String,
    card: Card,
}

#[derive(Serialize)]
struct Day {
    landed: Vec<Landed>,
    blocked: Vec<Landed>,
}

#[derive(Deserialize)]
struct ConfigQuery {
    #[serde(default)]
    repo: String,
}

#[derive(Deserialize)]
struct ConfigWrite {
    #[serde(default)]
    repo: String,
    text: String,
    version: String,
}

#[derive(Serialize)]
struct ConfigFile {
    repo: String,
    path: String,
    text: String,
    version: String,
}

#[derive(Deserialize)]
struct FeedQuery {
    #[serde(default)]
    repo: String,
}

#[derive(Deserialize)]
struct IdQuery {
    id: i64,
}

#[derive(Deserialize)]
struct CodeQuery {
    code: Option<String>,
}

#[derive(Serialize)]
struct LoginCode {
    code: String,
}

#[derive(Serialize)]
struct Project {
    repo: String,
    counts: Vec<(String, i64)>,
}

#[derive(Serialize)]
struct Card {
    id: i64,
    repo: String,
    number: Option<String>,
    title: String,
    status: String,
    agent: Option<String>,
    model: Option<String>,
    complexity: String,
    url: String,
    pr_number: Option<i32>,
    pr_url: Option<String>,
    labels: Vec<String>,
    block_reason: Option<String>,
    last_error: String,
    attempts: i32,
    review_cycles: i32,
    created_at: String,
    updated_at: String,
    /// When the task entered its status, filled in where a page shows how long it waited
    since: Option<String>,
}

#[derive(Serialize)]
struct Detail {
    card: Card,
    body: String,
    summary: String,
    branch: String,
    activity: Vec<Activity>,
    runs: Vec<RunRow>,
}

#[derive(Serialize)]
struct RunRow {
    attempt: i32,
    run_type: String,
    agent: String,
    model: String,
    outcome: String,
    error: String,
    duration_secs: f64,
    started_at: String,
    completed_at: Option<String>,
    input_tokens: i64,
    output_tokens: i64,
}

impl RunRow {
    // The prompt, environment and output stay out, they are large and can hold secrets
    fn from_run(r: TaskRun) -> Self {
        Self {
            attempt: r.attempt,
            run_type: r.run_type,
            agent: r.agent,
            model: r.model,
            outcome: r.outcome,
            error: security::redact(&r.error)
                .chars()
                .take(ERROR_CHARS)
                .collect(),
            duration_secs: r.duration_secs,
            started_at: r.started_at,
            completed_at: r.completed_at,
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
        }
    }
}

#[derive(Serialize)]
struct PrCheck {
    name: String,
    status: String,
    conclusion: Option<String>,
}

#[derive(Serialize)]
struct PrReview {
    login: String,
    state: String,
    submitted_at: String,
}

#[derive(Serialize)]
struct PrState {
    number: u64,
    title: String,
    state: String,
    merged: bool,
    mergeable: Option<bool>,
    mergeable_state: Option<String>,
    html_url: String,
    head: String,
    base: String,
    checks: Vec<PrCheck>,
    reviews: Vec<PrReview>,
}

#[derive(Serialize)]
struct QuotaWindow {
    agent: String,
    window: String,
    status: String,
    utilization: Option<f64>,
    resets_at: Option<i64>,
}

#[derive(Serialize)]
struct Cooldown {
    key: String,
    until: i64,
    reason: String,
}

#[derive(Serialize)]
struct Usage {
    windows: Vec<QuotaWindow>,
    cooldowns: Vec<Cooldown>,
}

#[derive(Deserialize)]
struct StatsQuery {
    days: u32,
}

#[derive(Serialize)]
struct Activity {
    timestamp: String,
    event_type: String,
    from_status: Option<String>,
    to_status: Option<String>,
    agent: Option<String>,
    model: Option<String>,
}

#[derive(Serialize)]
pub(super) struct ChatMessage {
    role: String,
    content: String,
    agent: Option<String>,
    model: Option<String>,
    created_at: String,
}

/// One line of an activity feed: a task changed status, or an agent wrote output
#[derive(Serialize)]
struct FeedItem {
    kind: &'static str,
    timestamp: String,
    repo: String,
    task: Option<i64>,
    number: Option<String>,
    title: String,
    from: Option<String>,
    to: Option<String>,
    agent: Option<String>,
    text: String,
}

/// An agent being followed by a feed, and the output file it writes now
struct Followed {
    task: Task,
    dirs: Vec<PathBuf>,
    tail: Option<(PathBuf, Tail, Formatter)>,
}

impl FeedItem {
    fn from_change(c: StatusChange) -> Self {
        Self {
            kind: "status",
            timestamp: c.timestamp,
            repo: c.repo,
            task: Some(c.task_id),
            number: c.external_id.filter(|n| !n.starts_with("internal:")),
            title: c.title,
            from: c.from_status,
            to: c.to_status,
            agent: c.agent,
            text: String::new(),
        }
    }
}

impl Card {
    fn from_task(t: Task) -> Self {
        let pr_url = t
            .pr_number
            .map(|n| format!("https://github.com/{}/pull/{n}", t.repo));
        let last_error: String = security::redact(&t.last_error)
            .chars()
            .take(ERROR_CHARS)
            .collect();
        Self {
            id: t.id,
            repo: t.repo,
            number: t.external_id.filter(|n| !n.starts_with("internal:")),
            title: t.title,
            status: t.status.as_str().to_string(),
            agent: t.agent,
            model: t.model,
            complexity: t.complexity,
            url: t.url,
            pr_number: t.pr_number,
            pr_url,
            labels: t.labels,
            block_reason: t.block_reason.map(|r| security::redact(&r)),
            last_error,
            attempts: t.attempts,
            review_cycles: t.review_cycles,
            created_at: t.created_at,
            updated_at: t.updated_at,
            since: None,
        }
    }
}

/// Cards for tasks, each with when it entered its status
async fn cards(store: &TaskStore, tasks: Vec<Task>) -> Vec<Card> {
    let ids: Vec<i64> = tasks.iter().map(|t| t.id).collect();
    let mut since = store.entered_status(&ids).await.unwrap_or_default();
    tasks
        .into_iter()
        .map(|t| {
            let entered = since.remove(&t.id);
            let mut card = Card::from_task(t);
            card.since = entered;
            card
        })
        .collect()
}

fn port_path() -> anyhow::Result<PathBuf> {
    Ok(home::state_dir()?.join("ui.port"))
}

/// Serve the UI on 127.0.0.1 and record the port, a fresh token and the pid for `orch ui`
pub async fn serve(
    store: Arc<TaskStore>,
    events: broadcast::Sender<TaskEvent>,
) -> anyhow::Result<u16> {
    let endpoint = serve_on(store, events).await?;
    write_private(
        &port_path()?,
        &format!("{} {} {}", endpoint.port, endpoint.token, process::id()),
    )?;
    Ok(endpoint.port)
}

/// Serve the UI without recording it, for `orch ui --dev`
pub async fn serve_on(
    store: Arc<TaskStore>,
    events: broadcast::Sender<TaskEvent>,
) -> anyhow::Result<Endpoint> {
    let listener = match TcpListener::bind(("127.0.0.1", PORT)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(port = PORT, err = %e, "web UI port taken, using a free one");
            TcpListener::bind("127.0.0.1:0").await?
        }
    };
    let port = listener.local_addr()?.port();
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let app = router(App {
        store,
        events,
        token: Arc::from(token.as_str()),
        host: Arc::from(format!("127.0.0.1:{port}")),
        codes: Arc::default(),
    });
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!(err = %e, "web UI stopped");
        }
    });
    tracing::info!(port, "web UI started on 127.0.0.1");
    Ok(Endpoint { port, token })
}

fn router(app: App) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/login-code", post(login_code))
        .route("/app.css", get(css))
        .route("/app.js", get(js))
        .route("/api/projects", get(projects))
        .route("/api/tasks", get(tasks))
        .route("/api/running", get(running))
        .route("/api/inbox", get(inbox))
        .route("/api/agents", get(compose::agents))
        .route("/api/plan", get(plan::state))
        .route("/api/plan/message", post(plan::message))
        .route("/api/plan/preview", post(plan::preview))
        .route("/api/plan/import", post(plan::import))
        .route("/api/plan/clear", post(plan::clear))
        .route("/api/tasks/new", post(compose::create))
        .route("/api/setup", get(setup::status))
        .route(
            "/api/setup/login",
            get(setup::login_state).post(setup::login_start),
        )
        .route("/api/setup/repos", get(setup::repos))
        .route("/api/setup/owners", get(setup::owners))
        .route("/api/setup/dirs", get(setup::dirs))
        .route("/api/projects/add", post(setup::add))
        .route("/api/pr", get(pr_state))
        .route("/api/usage", get(usage))
        .route("/api/stats", get(stats))
        .route("/api/day", get(day))
        .route("/api/task", get(task_detail))
        .route("/api/config", get(config_read).put(config_write))
        .route("/api/chat", get(chat::history).post(chat::send))
        .route("/api/events", get(events))
        .route("/api/output", get(output))
        .route("/api/logs", get(logs))
        .route("/api/activity", get(activity))
        .with_state(app)
}

pub async fn cleanup() {
    if let Ok(path) = port_path() {
        let _ = tokio_fs::remove_file(path).await;
    }
}

/// The running daemon's UI. A file left by a daemon that is gone names nothing
pub async fn endpoint() -> Option<Endpoint> {
    let body = tokio_fs::read_to_string(port_path().ok()?).await.ok()?;
    let mut parts = body.split_whitespace();
    let port: u16 = parts.next()?.parse().ok()?;
    let token = parts.next()?.to_string();
    let pid: u32 = parts.next()?.parse().ok()?;
    let alive = task::spawn_blocking(move || home::pid_is_orch_serve(pid))
        .await
        .ok()?;
    alive.then_some(Endpoint { port, token })
}

// A rebound DNS name sends its own Host, so only the literal address passes
fn host_ok(app: &App, headers: &HeaderMap) -> bool {
    headers
        .get(header::HOST)
        .is_some_and(|h| h.as_bytes() == app.host.as_bytes())
}

fn token_ok(app: &App, given: &str) -> bool {
    given.as_bytes().ct_eq(app.token.as_bytes()).into()
}

fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| pair.trim().strip_prefix("orch_ui="))
}

/**
 * Whether the request comes from the UI's own page or a direct visit. A cookie reaches every port of
 * 127.0.0.1, so a page another local server serves would carry it too. Browsers say where a request
 * started in `Sec-Fetch-Site` and `Origin`, and a tool that sends neither, such as curl, is the owner
 **/
fn same_origin(app: &App, headers: &HeaderMap) -> bool {
    let site_ok = headers
        .get("sec-fetch-site")
        .is_none_or(|v| matches!(v.as_bytes(), b"same-origin" | b"none"));
    let origin_ok = headers
        .get(header::ORIGIN)
        .is_none_or(|o| o.as_bytes() == format!("http://{}", app.host).as_bytes());
    site_ok && origin_ok
}

fn authorized(app: &App, headers: &HeaderMap) -> bool {
    host_ok(app, headers)
        && same_origin(app, headers)
        && cookie_token(headers).is_some_and(|t| token_ok(app, t))
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        "Run `orch ui` to open the web UI\n",
    )
        .into_response()
}

/// `orch ui` opens `/?token=…` once, which trades the token for a cookie and drops it from the URL
/**
 * A sign-in code for `orch ui`, which proves it can read the token file with a Bearer header. The
 * browser opens `/?code=…`, so its history keeps a code that is spent within a minute, never the
 * token itself
 **/
async fn login_code(State(app): State<App>, headers: HeaderMap) -> Response {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if !host_ok(&app, &headers) || !bearer.is_some_and(|t| token_ok(&app, t)) {
        return unauthorized();
    }

    let code = uuid::Uuid::new_v4().simple().to_string();
    let mut codes = app.codes.lock().await;
    let now = Instant::now();
    codes.retain(|_, until| *until > now);
    codes.insert(code.clone(), now + LOGIN_CODE_TTL);
    Json(LoginCode { code }).into_response()
}

async fn index(State(app): State<App>, headers: HeaderMap, Query(q): Query<CodeQuery>) -> Response {
    if !host_ok(&app, &headers) || !same_origin(&app, &headers) {
        return unauthorized();
    }

    // A code works once, then the cookie carries the token
    if let Some(code) = q.code {
        let fresh = app
            .codes
            .lock()
            .await
            .remove(&code)
            .is_some_and(|until| until > Instant::now());
        if !fresh {
            return unauthorized();
        }

        let cookie = format!("{COOKIE}={}; HttpOnly; SameSite=Strict; Path=/", app.token);
        return ([(header::SET_COOKIE, cookie)], Redirect::to("/")).into_response();
    }

    if !authorized(&app, &headers) {
        return unauthorized();
    }

    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::CACHE_CONTROL, "no-store"),
        ],
        INDEX_HTML,
    )
        .into_response()
}

async fn css(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS).into_response()
}

async fn js(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        APP_JS,
    )
        .into_response()
}

fn internal(e: &anyhow::Error) -> Response {
    tracing::warn!(err = %e, "web UI request failed");
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response()
}

/// The project's directory, or None when the repo is not one orch runs
fn project_dir(repo: &str) -> Option<PathBuf> {
    config::get_projects_with_paths()
        .ok()?
        .into_iter()
        .find_map(|(r, path)| (r == repo).then_some(path))
}

async fn projects(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let repos = match config::get_projects_with_paths() {
        Ok(p) => p,
        Err(e) => return internal(&e),
    };
    let mut out = Vec::new();
    for (repo, _) in repos {
        let counts = app.store.count_by_status(&repo).await.unwrap_or_default();
        out.push(Project { repo, counts });
    }
    Json(out).into_response()
}

async fn tasks(State(app): State<App>, headers: HeaderMap, Query(q): Query<RepoQuery>) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    match app.store.list_for_board(&q.repo, DONE_ON_BOARD).await {
        Ok(tasks) => Json(cards(&app.store, tasks).await).into_response(),
        Err(e) => internal(&e),
    }
}

async fn running(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    match app.store.list_running().await {
        Ok(tasks) => Json(cards(&app.store, tasks).await).into_response(),
        Err(e) => internal(&e),
    }
}

/// What waits on a person: blocked tasks, and tasks kept from agents for a person to do
async fn inbox(State(app): State<App>, headers: HeaderMap, Query(q): Query<FeedQuery>) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let repo = (!q.repo.is_empty()).then_some(q.repo.as_str());
    let open = match app.store.list_open(repo).await {
        Ok(t) => t,
        Err(e) => return internal(&e),
    };
    let mut out = Inbox {
        blocked: Vec::new(),
        human: Vec::new(),
    };
    for task in cards_waiting(&app.store, open).await {
        if task.status == TaskStatus::Blocked.as_str() {
            out.blocked.push(task);
        } else {
            out.human.push(task);
        }
    }
    // The longest wait first, which is the one to look at
    out.blocked.sort_by(|a, b| a.since.cmp(&b.since));
    out.human.sort_by(|a, b| a.since.cmp(&b.since));
    Json(out).into_response()
}

/// Open tasks that wait on a person, so only those pay for the status lookup
async fn cards_waiting(store: &TaskStore, open: Vec<Task>) -> Vec<Card> {
    let waiting = open
        .into_iter()
        .filter(|t| {
            t.status == TaskStatus::Blocked
                || t.labels
                    .iter()
                    .any(|l| l == HUMAN_LABEL || l == NO_AGENT_LABEL)
        })
        .collect();
    cards(store, waiting).await
}

/// RFC 3339 in the store's own format, so text comparison orders it
fn store_instant(value: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(value).ok().map(|t| {
        t.with_timezone(&chrono::Utc)
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    })
}

/// Tasks that merged between two instants, which the page picks as the user's local day
async fn day(State(app): State<App>, headers: HeaderMap, Query(q): Query<DayQuery>) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let (Some(from), Some(to)) = (store_instant(&q.from), store_instant(&q.to)) else {
        return (StatusCode::BAD_REQUEST, "from and to must be RFC 3339").into_response();
    };
    let repo = (!q.repo.is_empty()).then_some(q.repo.as_str());
    let landed = |rows: Vec<(String, Task)>| -> Vec<Landed> {
        rows.into_iter()
            .map(|(at, task)| Landed {
                at,
                card: Card::from_task(task),
            })
            .collect()
    };
    let done = app
        .store
        .entered_between(TaskStatus::Done, &from, &to, repo)
        .await;
    let blocked = app
        .store
        .entered_between(TaskStatus::Blocked, &from, &to, repo)
        .await;
    match (done, blocked) {
        (Ok(done), Ok(blocked)) => Json(Day {
            landed: landed(done),
            blocked: landed(blocked),
        })
        .into_response(),
        (Err(e), _) | (_, Err(e)) => internal(&e),
    }
}

/// The task's PR as GitHub has it now: state, mergeability, each check and each reviewer's latest say
async fn pr_state(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<IdQuery>,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let Ok(task) = app.store.get(q.id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(number) = task.pr_number.and_then(|n| u64::try_from(n).ok()) else {
        return StatusCode::NO_CONTENT.into_response();
    };
    match read_pr(&task.repo, number).await {
        Ok(pr) => Json(pr).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    }
}

async fn read_pr(repo: &str, number: u64) -> anyhow::Result<PrState> {
    let gh = GhHttp::new()?;
    let pr = gh.get_pr(repo, number).await?;
    let (checks, reviews) = tokio::join!(
        gh.get_check_runs(repo, &pr.head.sha),
        gh.get_pr_reviews(repo, number)
    );
    let mut latest: HashMap<String, PrReview> = HashMap::new();
    // A comment does not change a reviewer's verdict, so only these count
    for r in reviews.unwrap_or_default() {
        if matches!(
            r.state.as_str(),
            "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED"
        ) {
            latest.insert(
                r.user.login.clone(),
                PrReview {
                    login: r.user.login,
                    state: r.state,
                    submitted_at: r.submitted_at,
                },
            );
        }
    }
    Ok(PrState {
        number: pr.number,
        title: pr.title,
        state: pr.state,
        merged: pr.merged.unwrap_or(false),
        mergeable: pr.mergeable,
        mergeable_state: pr.mergeable_state,
        html_url: pr.html_url,
        head: pr.head.ref_,
        base: pr.base.ref_,
        checks: checks
            .unwrap_or_default()
            .into_iter()
            .map(|c| PrCheck {
                name: c.name,
                status: c.status,
                conclusion: c.conclusion,
            })
            .collect(),
        reviews: latest.into_values().collect(),
    })
}

/// Each agent's plan windows as the vendor last reported them, and every active cooldown
async fn usage(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let windows = cooldown::list_rate_limit_windows(&app.store)
        .await
        .into_iter()
        .map(|(agent, w)| QuotaWindow {
            agent,
            window: w.window_type,
            status: w.status,
            utilization: w.utilization,
            resets_at: w.resets_at,
        })
        .collect();
    let cooldowns = cooldown::list_persisted_cooldowns(&app.store)
        .await
        .into_iter()
        .map(|(key, until, reason)| Cooldown { key, until, reason })
        .collect();
    Json(Usage { windows, cooldowns }).into_response()
}

const STATS_MAX_DAYS: u32 = 90;

/// Merges, blocks and runs over the last `days`, counted by the page
async fn stats(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<StatsQuery>,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let days = i64::from(q.days.clamp(1, STATS_MAX_DAYS));
    let since = (chrono::Utc::now() - chrono::Duration::days(days))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let events: anyhow::Result<StatsEvents> = app.store.stats_since(&since).await;
    match events {
        Ok(e) => Json::<StatsEvents>(e).into_response(),
        Err(e) => internal(&e),
    }
}

async fn task_detail(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<IdQuery>,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let Ok(task) = app.store.get(q.id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let activity = app
        .store
        .get_activity(q.id, Some(200))
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|a| Activity {
            timestamp: a.timestamp,
            event_type: a.event_type,
            from_status: a.from_status,
            to_status: a.to_status,
            agent: a.agent,
            model: a.model,
        })
        .collect();
    let runs = app
        .store
        .get_runs(q.id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(RunRow::from_run)
        .collect();
    let body = security::redact(&task.body);
    let summary = security::redact(&task.summary);
    let branch = task.branch.clone();
    Json(Detail {
        card: Card::from_task(task),
        body,
        summary,
        branch,
        activity,
        runs,
    })
    .into_response()
}

/// The global config with no repo, else that project's `.orch.yml`
fn config_file(repo: &str) -> Option<PathBuf> {
    if repo.is_empty() {
        return home::config_path().ok();
    }

    project_dir(repo).map(|dir| dir.join(".orch.yml"))
}

/// Changes when the file does, so a save over someone else's edit is refused
fn version_of(text: &str) -> String {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

async fn config_read(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<ConfigQuery>,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let Some(path) = config_file(&q.repo) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let text = fs::read_to_string(&path).unwrap_or_default();
    Json(ConfigFile {
        repo: q.repo,
        path: path.display().to_string(),
        version: version_of(&text),
        text,
    })
    .into_response()
}

/// Why `text` cannot replace the config file for `repo`, if it cannot
fn config_problem(repo: &str, text: &str) -> Option<String> {
    let doc = match serde_norway::from_str::<serde_norway::Value>(text) {
        Ok(doc) => doc,
        Err(e) => return Some(format!("not valid YAML: {e}")),
    };
    if repo.is_empty() {
        return None;
    }

    // The engine finds a project by this key, so a save without it would drop the project
    let named = doc
        .get("gh")
        .and_then(|gh| gh.get("repo"))
        .and_then(|r| r.as_str());
    (named != Some(repo)).then(|| format!("gh.repo must stay {repo}"))
}

/**
 * Write `text` over a config file. A symlinked config, as in a dotfiles repo, is written at its
 * target so the link stays. The old text goes to orch's state dir, never beside the file, where a
 * project checkout would show it as untracked
 **/
fn save_config(path: &Path, current: &str, text: &str, backups: &Path) -> anyhow::Result<()> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !current.is_empty() {
        let name: String = target
            .to_string_lossy()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        write_private(&backups.join(format!("{name}.bak")), current)?;
    }

    write_private(&target, text)
}

/// Replace a config file as the owner edited it, keeping the old one in the state dir
async fn config_write(
    State(app): State<App>,
    headers: HeaderMap,
    Json(req): Json<ConfigWrite>,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let Some(path) = config_file(&req.repo) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let current = fs::read_to_string(&path).unwrap_or_default();
    if version_of(&current) != req.version {
        return (
            StatusCode::CONFLICT,
            "the file changed since you opened it, reload it first",
        )
            .into_response();
    }

    if let Some(problem) = config_problem(&req.repo, &req.text) {
        return (StatusCode::UNPROCESSABLE_ENTITY, problem).into_response();
    }

    let saved = home::state_dir()
        .and_then(|state| save_config(&path, &current, &req.text, &state.join("config-backups")));
    if let Err(e) = saved {
        return internal(&e);
    }

    // A save replaces the file, which can end a watch on its old inode, so it reloads here
    config::reloaded(&path);
    tracing::info!(path = %path.display(), "config saved from the web UI");
    Json(ConfigFile {
        repo: req.repo,
        path: path.display().to_string(),
        version: version_of(&req.text),
        text: req.text,
    })
    .into_response()
}

fn sse(rx: mpsc::Receiver<Event>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| (Ok(event), rx))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Task status changes as they happen. `lagged` tells the page to fetch the board again
async fn events(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<RepoQuery>,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let mut events = app.events.subscribe();
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        loop {
            let event = match events.recv().await {
                Ok(mut e) if q.repo.is_empty() || e.repo == q.repo => {
                    // Agent text can hold a secret, the activity feed redacts the same fields
                    e.error = e.error.map(|t| security::redact(&t));
                    e.summary = e.summary.map(|t| security::redact(&t));
                    e.review_context = e.review_context.map(|t| security::redact(&t));
                    Event::default().event("task").json_data(&e).ok()
                }
                Ok(_) => None,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    Some(Event::default().event("lagged").data(""))
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            if let Some(event) = event
                && tx.send(event).await.is_err()
            {
                break;
            }
        }
    });
    sse(rx).into_response()
}

/// The newest agent output of a task and its review, formatted the way `orch stream` prints it
async fn output(State(app): State<App>, headers: HeaderMap, Query(q): Query<IdQuery>) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let Ok(dirs) = task_ref::resolve(&app.store, &q.id.to_string())
        .await
        .and_then(|t| task_ref::state_dirs(&t))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(async move {
        let mut current: Option<(PathBuf, Tail, Formatter)> = None;
        loop {
            if let Some(path) = newest_output(&dirs)
                && current.as_ref().is_none_or(|(p, _, _)| *p != path)
            {
                let tail = Tail::open(path.clone(), false);
                current = Some((path, tail, Formatter::new(false)));
                if tx
                    .send(Event::default().event("attempt").data(""))
                    .await
                    .is_err()
                {
                    break;
                }
            }

            if let Some((_, tail, formatter)) = current.as_mut() {
                for line in tail.read_lines() {
                    for entry in formatter.format(&line) {
                        let event = Event::default()
                            .event("line")
                            .data(security::redact(&entry));
                        if tx.send(event).await.is_err() {
                            return;
                        }
                    }
                }
            }

            if tx.is_closed() {
                break;
            }

            time::sleep(TAIL_POLL).await;
        }
    });
    sse(rx).into_response()
}

/// The service log as it grows
async fn logs(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let path = match home::state_dir() {
        Ok(dir) => dir.join("orch.log"),
        Err(e) => return internal(&e),
    };
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(async move {
        let mut tail = Tail::open(path.clone(), false);
        loop {
            if tail.shrank() {
                tail = Tail::open(path.clone(), false);
            }

            for line in tail.read_lines() {
                let event = Event::default().event("line").data(security::redact(&line));
                if tx.send(event).await.is_err() {
                    return;
                }
            }

            if tx.is_closed() {
                break;
            }

            time::sleep(TAIL_POLL).await;
        }
    });
    sse(rx).into_response()
}

const FEED_BACKLOG: i64 = 60;
const FEED_LINE_CHARS: usize = 240;

fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn feed_event(item: &FeedItem) -> Option<Event> {
    Event::default().event("item").json_data(item).ok()
}

/// Status changes and agent output, for one project or every one
async fn activity(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<FeedQuery>,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let repo = (!q.repo.is_empty()).then_some(q.repo);
    let backlog = app
        .store
        .recent_status_changes(repo.as_deref(), FEED_BACKLOG)
        .await
        .unwrap_or_default();
    let (tx, rx) = mpsc::channel(512);
    for change in backlog {
        if let Some(event) = feed_event(&FeedItem::from_change(change)) {
            let _ = tx.try_send(event);
        }
    }

    tokio::spawn(feed_status(
        Arc::clone(&app.store),
        app.events.subscribe(),
        repo.clone(),
        tx.clone(),
    ));
    tokio::spawn(feed_output(app.store, repo, tx));
    sse(rx).into_response()
}

async fn feed_status(
    store: Arc<TaskStore>,
    mut events: broadcast::Receiver<TaskEvent>,
    repo: Option<String>,
    tx: mpsc::Sender<Event>,
) {
    loop {
        let e = match events.recv().await {
            Ok(e) => e,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        };
        if repo.as_ref().is_some_and(|r| *r != e.repo) || e.old_status == e.new_status {
            continue;
        }

        let task = store
            .get_by_external_id(&e.repo, &e.task_id)
            .await
            .ok()
            .flatten();
        let item = FeedItem {
            kind: "status",
            timestamp: e.timestamp,
            task: task.as_ref().map(|t| t.id),
            number: (!e.task_id.starts_with("internal:")).then_some(e.task_id),
            title: e
                .title
                .or_else(|| task.map(|t| t.title))
                .unwrap_or_default(),
            repo: e.repo,
            from: Some(e.old_status).filter(|s| !s.is_empty()),
            to: Some(e.new_status),
            agent: e.agent,
            text: e
                .error
                .map(|err| security::redact(&err))
                .unwrap_or_default(),
        };
        if let Some(event) = feed_event(&item)
            && tx.send(event).await.is_err()
        {
            break;
        }
    }
}

/// Follow every running agent's output, the way `orch stream` does with no task named
async fn feed_output(store: Arc<TaskStore>, repo: Option<String>, tx: mpsc::Sender<Event>) {
    let mut followed: HashMap<i64, Followed> = HashMap::new();
    let mut first = true;
    while !tx.is_closed() {
        let running = store.list_running().await.unwrap_or_default();
        let live: Vec<i64> = running.iter().map(|t| t.id).collect();
        for task in running {
            if repo.as_ref().is_some_and(|r| *r != task.repo) || followed.contains_key(&task.id) {
                continue;
            }

            let Ok(dirs) = task_ref::resolve(&store, &task.id.to_string())
                .await
                .and_then(|t| task_ref::state_dirs(&t))
            else {
                continue;
            };
            followed.insert(
                task.id,
                Followed {
                    task,
                    dirs,
                    tail: None,
                },
            );
        }

        for f in followed.values_mut() {
            if let Some(path) = newest_output(&f.dirs)
                && f.tail.as_ref().is_none_or(|(p, _, _)| *p != path)
            {
                // A feed opened mid-run starts at the end, a new attempt starts at its beginning
                let tail = if first {
                    Tail::open_at_end(path.clone())
                } else {
                    Tail::open(path.clone(), false)
                };
                f.tail = Some((path, tail, Formatter::new(false)));
            }

            let Some((_, tail, formatter)) = f.tail.as_mut() else {
                continue;
            };
            for line in tail.read_lines() {
                for entry in formatter.format(&line) {
                    let text: String = security::redact(entry.lines().next().unwrap_or(""))
                        .chars()
                        .take(FEED_LINE_CHARS)
                        .collect();
                    if text.trim().is_empty() {
                        continue;
                    }

                    let item = FeedItem {
                        kind: "output",
                        timestamp: now(),
                        repo: f.task.repo.clone(),
                        task: Some(f.task.id),
                        number: f
                            .task
                            .external_id
                            .clone()
                            .filter(|n| !n.starts_with("internal:")),
                        title: f.task.title.clone(),
                        from: None,
                        to: None,
                        agent: f.task.agent.clone(),
                        text,
                    };
                    if let Some(event) = feed_event(&item)
                        && tx.send(event).await.is_err()
                    {
                        return;
                    }
                }
            }
        }

        // A finished task got its last read above
        followed.retain(|id, _| live.contains(id));
        first = false;
        time::sleep(TAIL_POLL).await;
    }
}

#[cfg(test)]
mod tests;
