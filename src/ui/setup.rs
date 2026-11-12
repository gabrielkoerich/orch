//! First run and adding projects: what `orch init` checks, GitHub sign-in through `gh`, the
//! user's repos and checkouts, a folder browser, and the three ways to add a project

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::task;
use tokio::time;

use super::{App, authorized, internal, unauthorized};
use crate::cli::init;
use crate::cli::{clone_over_https, parse_github_slug};
use crate::config;
use crate::github::token;
use crate::home;

const GH_TIMEOUT: Duration = Duration::from_secs(60);
const CODE_WAIT: Duration = Duration::from_secs(15);
const DEVICE_URL: &str = "https://github.com/login/device";
const LOGIN_OUTPUT_LINES: usize = 20;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
// Where people keep checkouts, two levels deep so `~/code/org/repo` is found too
const CHECKOUT_ROOTS: &[&str] = &[
    "Projects",
    "projects",
    "code",
    "Code",
    "src",
    "dev",
    "Developer",
    "repos",
    "git",
    "work",
    "workspace",
    "GitHub",
    "Documents/GitHub",
];

#[derive(Debug, Clone, Default, Serialize)]
struct Login {
    state: &'static str,
    code: Option<String>,
    url: &'static str,
    error: Option<String>,
    #[serde(skip)]
    output: Vec<String>,
}

static LOGIN: LazyLock<Mutex<Login>> = LazyLock::new(|| Mutex::new(Login::default()));

#[derive(Serialize)]
struct Setup {
    #[serde(flatten)]
    ready: init::Readiness,
    login: Option<String>,
    projects: usize,
}

#[derive(Serialize)]
struct RemoteRepo {
    repo: String,
    description: Option<String>,
    private: bool,
    pushed_at: Option<String>,
    added: bool,
    checkout: Option<String>,
}

#[derive(Serialize)]
struct Checkout {
    path: String,
    repo: String,
    added: bool,
}

#[derive(Serialize)]
struct Repos {
    github: Vec<RemoteRepo>,
    local: Vec<Checkout>,
}

#[derive(Deserialize)]
pub(super) struct DirQuery {
    #[serde(default)]
    path: String,
}

#[derive(Serialize)]
struct Dir {
    name: String,
    path: String,
    repo: Option<String>,
}

#[derive(Serialize)]
struct Folder {
    path: String,
    parent: Option<String>,
    dirs: Vec<Dir>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum AddProject {
    Clone {
        repo: String,
        #[serde(default)]
        path: String,
    },
    Existing {
        path: String,
    },
    Create {
        owner: String,
        name: String,
        #[serde(default)]
        description: String,
        private: bool,
    },
}

#[derive(Serialize)]
struct Added {
    repo: String,
}

async fn gh(args: &[&str]) -> anyhow::Result<String> {
    let out = time::timeout(
        GH_TIMEOUT,
        Command::new("gh").args(args).kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("gh {} took too long", args.join(" ")))??;
    if !out.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn registered_repos() -> Vec<String> {
    config::get_projects_with_paths()
        .map(|p| p.into_iter().map(|(repo, _)| repo).collect())
        .unwrap_or_default()
}

pub(super) async fn status(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let Ok(ready) = task::spawn_blocking(init::readiness).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let login = if ready.gh == "ready" {
        gh(&["api", "user", "--jq", ".login"]).await.ok()
    } else {
        None
    };
    Json(Setup {
        ready,
        login,
        projects: registered_repos().len(),
    })
    .into_response()
}

/// Start `gh auth login --web` and hand back its one-time code, or report the running one
pub(super) async fn login_start(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    // One lock covers the check and the start, so two quick clicks start one sign-in
    {
        let mut current = LOGIN.lock().await;
        if current.state == "waiting" {
            return Json(current.clone()).into_response();
        }

        *current = Login {
            state: "waiting",
            url: DEVICE_URL,
            ..Login::default()
        };
    }

    if let Err(e) = spawn_login().await {
        let mut current = LOGIN.lock().await;
        current.state = "failed";
        current.error = Some(format!("{e:#}"));
        return internal(&e);
    }

    // The code comes a moment after the start, so wait for it here and save the page a poll
    let deadline = time::Instant::now() + CODE_WAIT;
    loop {
        let current = LOGIN.lock().await.clone();
        if current.code.is_some() || current.state != "waiting" || time::Instant::now() > deadline {
            return Json(current).into_response();
        }

        time::sleep(Duration::from_millis(200)).await;
    }
}

pub(super) async fn login_state(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    Json(LOGIN.lock().await.clone()).into_response()
}

fn device_code(line: &str) -> Option<String> {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .find(|w| {
            w.len() == 9
                && w.as_bytes()[4] == b'-'
                && w.chars()
                    .all(|c| c == '-' || c.is_ascii_uppercase() || c.is_ascii_digit())
        })
        .map(str::to_string)
}

// gh prints the code on stderr without a terminal and on stdout with one, so both are read
async fn read_code(stream: impl AsyncRead + Unpin) {
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut login = LOGIN.lock().await;
        if let Some(code) = device_code(&line) {
            login.code = Some(code);
        }

        // gh's last words are the error a failed sign-in shows
        login.output.push(line);
        if login.output.len() > LOGIN_OUTPUT_LINES {
            login.output.remove(0);
        }
    }
}

async fn spawn_login() -> anyhow::Result<()> {
    let mut child = Command::new("gh")
        .args([
            "auth",
            "login",
            "--hostname",
            "github.com",
            "--web",
            "--git-protocol",
            "https",
            "--skip-ssh-key",
        ])
        // The page opens the device URL itself, gh must not open a browser on the daemon's side
        .env("GH_BROWSER", "true")
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"\n").await;
    }

    if let Some(err) = child.stderr.take() {
        tokio::spawn(read_code(err));
    }

    if let Some(out) = child.stdout.take() {
        tokio::spawn(read_code(out));
    }

    tokio::spawn(async move {
        // A sign-in nobody finishes ends here, so the next click can start a new one
        let Ok(done) = time::timeout(LOGIN_TIMEOUT, child.wait()).await else {
            let _ = child.kill().await;
            let mut login = LOGIN.lock().await;
            login.state = "failed";
            login.error = Some("sign-in timed out, start it again".to_string());
            return;
        };
        if matches!(&done, Ok(status) if status.success()) {
            // Git pushes over HTTPS then use the same sign-in, and orch reads the new token. Done
            // without the lock, so the page's status checks are not held while gh runs
            let _ = gh(&["auth", "setup-git"]).await;
            token::shared().clear_cache().await;
        }

        let mut login = LOGIN.lock().await;
        match done {
            Ok(status) if status.success() => login.state = "done",
            Ok(_) => {
                login.state = "failed";
                login.error = Some(login.output.join("\n"));
            }
            Err(e) => {
                login.state = "failed";
                login.error = Some(e.to_string());
            }
        }
    });
    Ok(())
}

/// The `origin` of a checkout from `.git/config`, without running git for every folder
fn origin_of(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join(".git").join("config")).ok()?;
    let mut in_origin = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_origin = line == "[remote \"origin\"]";
        } else if in_origin && let Some(url) = line.strip_prefix("url") {
            let url = url.trim_start().trim_start_matches('=').trim();
            let (owner, name) = parse_github_slug(url)?;
            return Some(format!("{owner}/{name}"));
        }
    }
    None
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && !p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .collect()
}

fn scan_checkouts() -> Vec<(PathBuf, String)> {
    let Some(home) = dirs_home() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for root in CHECKOUT_ROOTS {
        for one in subdirs(&home.join(root)) {
            if let Some(repo) = origin_of(&one) {
                found.push((one, repo));
                continue;
            }
            for two in subdirs(&one) {
                if let Some(repo) = origin_of(&two) {
                    found.push((two, repo));
                }
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

fn dirs_home() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

/// The user's GitHub repos, most recently pushed first, each with a checkout found on this computer
pub(super) async fn repos(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let added = registered_repos();
    let local = task::spawn_blocking(scan_checkouts)
        .await
        .unwrap_or_default();
    let mut by_repo: HashMap<String, String> = HashMap::new();
    for (path, repo) in &local {
        by_repo
            .entry(repo.to_lowercase())
            .or_insert_with(|| path.display().to_string());
    }

    let listed = gh(&[
        "api",
        "user/repos?sort=pushed&per_page=100&affiliation=owner,collaborator,organization_member",
        "--jq",
        "[.[] | {repo: .full_name, description, private, pushed_at}]",
    ])
    .await;
    let github = match listed {
        Ok(text) => serde_json::from_str::<Vec<serde_json::Value>>(&text)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| {
                let repo = v.get("repo")?.as_str()?.to_string();
                Some(RemoteRepo {
                    added: added.iter().any(|a| a.eq_ignore_ascii_case(&repo)),
                    checkout: by_repo.get(&repo.to_lowercase()).cloned(),
                    description: v
                        .get("description")
                        .and_then(|d| d.as_str())
                        .map(str::to_string),
                    private: v
                        .get("private")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                    pushed_at: v
                        .get("pushed_at")
                        .and_then(|d| d.as_str())
                        .map(str::to_string),
                    repo,
                })
            })
            .collect(),
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    };
    let local = local
        .into_iter()
        .map(|(path, repo)| Checkout {
            added: added.iter().any(|a| a.eq_ignore_ascii_case(&repo)),
            path: path.display().to_string(),
            repo,
        })
        .collect();
    Json(Repos { github, local }).into_response()
}

/// Accounts a new repo can go under: the user, then their organizations
pub(super) async fn owners(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let user = gh(&["api", "user", "--jq", ".login"]).await;
    let orgs = gh(&["api", "user/orgs", "--jq", ".[].login"])
        .await
        .unwrap_or_default();
    match user {
        Ok(user) => {
            let mut all = vec![user];
            all.extend(orgs.lines().map(str::to_string).filter(|o| !o.is_empty()));
            Json(all).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    }
}

/// One folder's subfolders, for the Choose… dialog. A browser cannot hand the daemon a path
pub(super) async fn dirs(
    State(app): State<App>,
    headers: HeaderMap,
    Query(q): Query<DirQuery>,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    let Some(home) = dirs_home() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let dir = if q.path.is_empty() {
        home
    } else {
        PathBuf::from(&q.path)
    };
    let listed = task::spawn_blocking(move || {
        let mut dirs: Vec<Dir> = subdirs(&dir)
            .into_iter()
            .map(|p| Dir {
                name: p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                repo: origin_of(&p),
                path: p.display().to_string(),
            })
            .collect();
        dirs.sort_by_key(|d| d.name.to_lowercase());
        Folder {
            parent: dir.parent().map(|p| p.display().to_string()),
            path: dir.display().to_string(),
            dirs,
        }
    })
    .await;
    match listed {
        Ok(d) => Json(d).into_response(),
        Err(e) => internal(&anyhow::anyhow!(e)),
    }
}

/// Clone, add an existing checkout, or create a repo, then register it as `orch init` does
pub(super) async fn add(
    State(app): State<App>,
    headers: HeaderMap,
    Json(req): Json<AddProject>,
) -> Response {
    if !authorized(&app, &headers) {
        return unauthorized();
    }

    // Cloning takes a while, so the work goes on even if the page closes before it ends
    let work = tokio::spawn(add_project(req));
    match work.await.unwrap_or_else(|e| Err(anyhow::anyhow!(e))) {
        Ok(repo) => {
            // The engine starts a project when the config changes, a first-run file was not watched
            if let Ok(path) = home::config_path() {
                config::reloaded(&path);
            }
            Json(Added { repo }).into_response()
        }
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")).into_response(),
    }
}

async fn add_project(req: AddProject) -> anyhow::Result<String> {
    let input = match req {
        AddProject::Clone { repo, path } => {
            let (owner, name) = parse_github_slug(&repo)
                .ok_or_else(|| anyhow::anyhow!("{repo} is not owner/name"))?;
            let slug = format!("{owner}/{name}");
            let path = path.trim().to_string();
            if path.is_empty() {
                slug
            } else {
                let dest = PathBuf::from(&path);
                if !dest.exists() {
                    let out = Command::from(clone_over_https(&slug, &dest, false))
                        .kill_on_drop(true)
                        .output()
                        .await?;
                    if !out.status.success() {
                        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
                    }
                } else if origin_of(&dest).as_deref() != Some(slug.as_str()) {
                    anyhow::bail!("{path} already exists and is not a checkout of {slug}");
                }
                path
            }
        }
        AddProject::Existing { path } => path.trim().to_string(),
        AddProject::Create {
            owner,
            name,
            description,
            private,
        } => {
            let slug = format!("{}/{}", owner.trim(), name.trim());
            let visibility = if private { "--private" } else { "--public" };
            gh(&[
                "repo",
                "create",
                &slug,
                visibility,
                "--add-readme",
                "--description",
                &description,
            ])
            .await?;
            slug
        }
    };
    task::spawn_blocking(move || init::add_project_from_ui(&input)).await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_device_code_is_found_in_gh_output() {
        assert_eq!(
            device_code("! One-time code (AB12-CD34) copied to clipboard").as_deref(),
            Some("AB12-CD34"),
            "the code gh prints without a terminal"
        );
        assert_eq!(
            device_code("! First copy your one-time code: 9F3A-77QK").as_deref(),
            Some("9F3A-77QK"),
            "the code gh prints in a terminal"
        );
        assert!(
            device_code(
                "Open this URL to continue in your web browser: https://github.com/login/device"
            )
            .is_none(),
            "the URL line has no code"
        );
    }

    #[test]
    fn a_checkout_origin_is_read_from_git_config() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(
            dir.path().join(".git").join("config"),
            "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = git@github.com:acme/widgets.git\n",
        )
        .unwrap();
        assert_eq!(
            origin_of(dir.path()).as_deref(),
            Some("acme/widgets"),
            "ssh origin"
        );
    }
}
