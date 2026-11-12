//! First-run setup: probe `gh` and agent CLIs, write a minimal config, start the service

use crate::cmd::SyncCommandErrorContext;
use crate::cmd_cache;
use crate::config::projects_yaml;
use crate::engine::router::config::DEFAULT_AGENTS;
use crate::home;
use anyhow::Context;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Login-status CLIs that hang get killed after this
const LOGIN_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);

const DEFAULT_ROUTER_AGENT: &str = "claude";
const DEFAULT_FALLBACK_EXECUTOR: &str = "codex";

/// Next command printed after a successful first run
const NEXT_COMMAND: &str = "orch plan <file>";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GhStatus {
    Missing,
    LoggedOut,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentPresence {
    Missing,
    LoggedOut,
    Ready,
}

trait Host {
    fn gh_status(&self) -> GhStatus;
    fn agent_status(&self, name: &str) -> AgentPresence;
    fn git_origin_slug(&self) -> Option<String>;
    fn prompt_line(&self, prompt: &str) -> anyhow::Result<String>;
    fn config_exists(&self) -> bool;
    fn config_path(&self) -> PathBuf;
    fn write_config(&self, contents: &str) -> anyhow::Result<()>;
    fn local_checkout_of(&self, slug: &str) -> bool;
    fn ensure_local_project_yml(&self, slug: &str) -> anyhow::Result<()>;
    fn add_project(&self, input: &str) -> anyhow::Result<PathBuf>;
    fn start_service(&self) -> anyhow::Result<()>;
    fn print(&self, line: &str);
}

struct Live;

impl Host for Live {
    fn gh_status(&self) -> GhStatus {
        if !cmd_cache::command_exists("gh") {
            return GhStatus::Missing;
        }
        if env_nonempty("GH_TOKEN") || env_nonempty("GITHUB_TOKEN") {
            return GhStatus::Ready;
        }
        match run_limited("gh", &["auth", "status"], LOGIN_COMMAND_TIMEOUT) {
            Some((ok, stdout, stderr)) if output_says_logged_in(ok, &stdout, &stderr) => {
                GhStatus::Ready
            }
            _ => GhStatus::LoggedOut,
        }
    }

    fn agent_status(&self, name: &str) -> AgentPresence {
        if !cmd_cache::command_exists(name) {
            return AgentPresence::Missing;
        }
        if env_logged_in(name) || credential_file_logged_in(name) {
            return AgentPresence::Ready;
        }
        let Some(args) = login_args(name) else {
            return AgentPresence::LoggedOut;
        };
        match run_limited(name, args, LOGIN_COMMAND_TIMEOUT) {
            Some((ok, stdout, stderr)) if output_says_logged_in(ok, &stdout, &stderr) => {
                AgentPresence::Ready
            }
            _ => AgentPresence::LoggedOut,
        }
    }

    fn git_origin_slug(&self) -> Option<String> {
        read_git_origin_slug()
    }

    fn prompt_line(&self, prompt: &str) -> anyhow::Result<String> {
        if !io::stdin().is_terminal() {
            return Ok(String::new());
        }
        print!("{prompt}");
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        Ok(line)
    }

    fn config_exists(&self) -> bool {
        home::config_path().is_ok_and(|path| path.exists())
    }

    fn config_path(&self) -> PathBuf {
        home::config_path().unwrap_or_else(|_| PathBuf::from("config.yml"))
    }

    fn write_config(&self, contents: &str) -> anyhow::Result<()> {
        let path = home::config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))
    }

    fn local_checkout_of(&self, slug: &str) -> bool {
        read_git_origin_slug().as_deref() == Some(slug)
    }

    fn ensure_local_project_yml(&self, slug: &str) -> anyhow::Result<()> {
        let path = Path::new(".orch.yml");
        if path.exists() {
            return Ok(());
        }
        fs::write(path, format!("gh:\n  repo: \"{slug}\"\n"))
            .with_context(|| format!("writing {}", path.display()))?;
        self.print("Created .orch.yml");
        Ok(())
    }

    fn add_project(&self, input: &str) -> anyhow::Result<PathBuf> {
        super::project_add(input)?;
        registered_path(input)
    }

    fn start_service(&self) -> anyhow::Result<()> {
        super::service::start()
    }

    fn print(&self, line: &str) {
        println!("{line}");
    }
}

/// Detect agents, write a minimal config, and start the service
pub(super) fn run(repo: Option<String>) -> anyhow::Result<()> {
    let _ = home::orch_home()?;
    run_with(&Live, repo)
}

fn run_with(host: &dyn Host, repo: Option<String>) -> anyhow::Result<()> {
    host.print("Checking GitHub and agent CLIs");
    let gh = host.gh_status();
    host.print(&format!("  {:<10} {}", "gh", gh_label(gh)));
    let mut ready = Vec::new();
    for name in DEFAULT_AGENTS {
        let status = host.agent_status(name);
        host.print(&format!("  {:<10} {}", name, agent_label(status)));
        if status == AgentPresence::Ready {
            ready.push((*name).to_string());
        }
    }
    match gh {
        GhStatus::Missing => {
            anyhow::bail!("gh is not on PATH. Install the GitHub CLI and run `gh auth login`")
        }
        GhStatus::LoggedOut => anyhow::bail!("GitHub CLI is not logged in. Run `gh auth login`"),
        GhStatus::Ready => {}
    }
    if ready.is_empty() {
        anyhow::bail!(
            "no agent CLI is logged in. Install one (claude, codex, opencode, grok) and log in, then run `orch init` again"
        );
    }
    host.print(&format!("Using {}", ready.join(", ")));

    let slug = resolve_slug(host, repo)?;
    host.print(&format!("Repository: {slug}"));

    let existed = host.config_exists();
    let add_input = if host.local_checkout_of(&slug) {
        host.ensure_local_project_yml(&slug)?;
        "."
    } else {
        slug.as_str()
    };
    let project_path = host.add_project(add_input)?;
    if !existed {
        let yaml = render_minimal_config(&project_path.to_string_lossy(), &ready);
        host.write_config(&yaml)?;
        host.print(&format!("Wrote {}", host.config_path().display()));
    }

    host.start_service().with_context(
        || "wrote the config but could not start the service. Start it with: orch service start",
    )?;
    host.print("Started the orch service");
    host.print("");
    host.print(&format!("Next: {NEXT_COMMAND}"));
    Ok(())
}

fn resolve_slug(host: &dyn Host, repo: Option<String>) -> anyhow::Result<String> {
    if let Some(raw) = repo {
        return parse_slug_or_err(&raw);
    }
    let origin = host.git_origin_slug();
    let prompt = match &origin {
        Some(slug) => format!("Repository [{slug}]: "),
        None => "Repository (owner/repo): ".to_string(),
    };
    let line = host.prompt_line(&prompt)?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return origin
            .ok_or_else(|| anyhow::anyhow!("a repository is required. Use --repo OWNER/REPO"));
    }
    parse_slug_or_err(trimmed)
}

fn parse_slug_or_err(raw: &str) -> anyhow::Result<String> {
    super::parse_github_slug(raw)
        .map(|(owner, name)| format!("{owner}/{name}"))
        .ok_or_else(|| anyhow::anyhow!("repository must be OWNER/REPO (or a GitHub URL)"))
}

fn read_git_origin_slug() -> Option<String> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output_with_context()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout);
    let (owner, name) = super::parse_github_slug(url.trim())?;
    Some(format!("{owner}/{name}"))
}

fn registered_path(input: &str) -> anyhow::Result<PathBuf> {
    if let Some((owner, repo)) = super::parse_github_slug(input) {
        Ok(home::projects_dir()?
            .join(owner)
            .join(format!("{repo}.git")))
    } else if input == "." {
        env::current_dir().context("current directory")
    } else {
        PathBuf::from(input)
            .canonicalize()
            .with_context(|| format!("resolving {input}"))
    }
}

fn render_minimal_config(project: &str, ready: &[String]) -> String {
    let mut out = String::from("projects:\n");
    out.push_str("  - ");
    out.push_str(&projects_yaml::format_yaml_string(project));
    out.push('\n');
    if !ready_matches_defaults(ready) {
        out.push('\n');
        out.push_str("agents:\n");
        for agent in ready {
            out.push_str("  - ");
            out.push_str(&projects_yaml::format_yaml_string(agent));
            out.push('\n');
        }
    }
    let router_agent = router_agent_override(ready);
    let fallback = fallback_override(ready);
    if router_agent.is_some() || fallback.is_some() {
        out.push('\n');
        out.push_str("router:\n");
        if let Some(agent) = router_agent {
            out.push_str("  agent: ");
            out.push_str(&projects_yaml::format_yaml_string(agent));
            out.push('\n');
        }
        if let Some(agent) = fallback {
            out.push_str("  fallback_executor: ");
            out.push_str(&projects_yaml::format_yaml_string(agent));
            out.push('\n');
        }
    }
    out
}

fn ready_matches_defaults(ready: &[String]) -> bool {
    ready.len() == DEFAULT_AGENTS.len()
        && ready.iter().zip(DEFAULT_AGENTS.iter()).all(|(a, d)| a == d)
}

fn router_agent_override(ready: &[String]) -> Option<&str> {
    if ready.iter().any(|a| a == DEFAULT_ROUTER_AGENT) {
        None
    } else {
        ready.first().map(String::as_str)
    }
}

fn fallback_override(ready: &[String]) -> Option<&str> {
    if ready.iter().any(|a| a == DEFAULT_FALLBACK_EXECUTOR) {
        None
    } else {
        ready.first().map(String::as_str)
    }
}

fn gh_label(status: GhStatus) -> &'static str {
    match status {
        GhStatus::Missing => "missing",
        GhStatus::LoggedOut => "not logged in",
        GhStatus::Ready => "ok",
    }
}

fn agent_label(status: AgentPresence) -> &'static str {
    match status {
        AgentPresence::Missing => "missing",
        AgentPresence::LoggedOut => "not logged in",
        AgentPresence::Ready => "logged in",
    }
}

/// Status commands that exit without opening a session. Grok and OpenCode have none
fn login_args(agent: &str) -> Option<&'static [&'static str]> {
    match agent {
        "claude" => Some(&["auth", "status", "--text"]),
        "codex" => Some(&["login", "status"]),
        _ => None,
    }
}

fn login_env_vars(agent: &str) -> &'static [&'static str] {
    match agent {
        "claude" => &["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"],
        "codex" => &["OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_ACCESS_TOKEN"],
        "grok" => &["XAI_API_KEY"],
        "kimi" => &["KIMI_API_KEY"],
        "minimax" => &["MINIMAX_API_KEY"],
        "glm" => &["GLM_API_KEY"],
        _ => &[],
    }
}

fn env_logged_in(agent: &str) -> bool {
    login_env_vars(agent).iter().any(|key| env_nonempty(key))
}

fn env_nonempty(key: &str) -> bool {
    env::var(key).is_ok_and(|value| !value.is_empty())
}

fn credential_file_logged_in(agent: &str) -> bool {
    credential_paths(agent)
        .iter()
        .any(|path| fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() > 2))
}

fn credential_paths(agent: &str) -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let mut paths = credential_paths_in(&home, agent);
    if agent == "opencode" {
        push_xdg_opencode_auth(&mut paths, "XDG_DATA_HOME");
        push_xdg_opencode_auth(&mut paths, "XDG_CONFIG_HOME");
    }
    paths
}

fn credential_paths_in(home: &Path, agent: &str) -> Vec<PathBuf> {
    match agent {
        "claude" => vec![home.join(".claude").join(".credentials.json")],
        "codex" => vec![home.join(".codex").join("auth.json")],
        "opencode" => vec![
            home.join(".local")
                .join("share")
                .join("opencode")
                .join("auth.json"),
            home.join(".config").join("opencode").join("auth.json"),
        ],
        "grok" => vec![home.join(".grok").join("auth.json")],
        _ => Vec::new(),
    }
}

fn push_xdg_opencode_auth(paths: &mut Vec<PathBuf>, var: &str) {
    if let Ok(xdg) = env::var(var)
        && !xdg.is_empty()
    {
        paths.push(PathBuf::from(xdg).join("opencode").join("auth.json"));
    }
}

fn output_says_logged_in(success: bool, stdout: &str, stderr: &str) -> bool {
    if !success {
        return false;
    }
    !logged_out_phrase(&format!("{stdout}\n{stderr}"))
}

fn logged_out_phrase(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("not logged in")
        || lower.contains("not authenticated")
        || lower.contains("unauthenticated")
        || lower.contains("please log in")
        || lower.contains("please login")
        || lower.contains("run grok login")
}

fn run_limited(program: &str, args: &[&str], timeout: Duration) -> Option<(bool, String, String)> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = String::new();
                let mut stderr = String::new();
                if let Some(mut out) = child.stdout.take() {
                    let _ = out.read_to_string(&mut stdout);
                }
                if let Some(mut err) = child.stderr.take() {
                    let _ = err.read_to_string(&mut stderr);
                }
                return Some((status.success(), stdout, stderr));
            }
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
}

/// What the web UI's first-run screen shows: GitHub, each agent CLI, and whether a config exists
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Readiness {
    pub(crate) gh: &'static str,
    pub(crate) agents: Vec<(String, &'static str)>,
    pub(crate) config_exists: bool,
}

/// The checks `orch init` prints, as data. It runs CLIs with a timeout, so call it off the runtime
pub(crate) fn readiness() -> Readiness {
    let host = Live;
    let gh = match host.gh_status() {
        GhStatus::Missing => "missing",
        GhStatus::LoggedOut => "logged_out",
        GhStatus::Ready => "ready",
    };
    let agents = DEFAULT_AGENTS
        .iter()
        .map(|name| {
            let status = match host.agent_status(name) {
                AgentPresence::Missing => "missing",
                AgentPresence::LoggedOut => "logged_out",
                AgentPresence::Ready => "ready",
            };
            ((*name).to_string(), status)
        })
        .collect();
    Readiness {
        gh,
        agents,
        config_exists: host.config_exists(),
    }
}

/// The GitHub repo a local checkout pushes to, from its `origin`
pub(crate) fn origin_slug_of(dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["remote", "get-url", "origin"])
        .output_with_context()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout);
    let (owner, name) = super::parse_github_slug(url.trim())?;
    Some(format!("{owner}/{name}"))
}

/**
 * Add a project the way `orch init` does, from the web UI: a GitHub slug gets a bare clone, a
 * local checkout gets a `.orch.yml` naming its origin. With no config yet, the first project
 * writes the minimal config with the agents that are ready, as `orch init` would
 **/
pub(crate) fn add_project_from_ui(input: &str) -> anyhow::Result<String> {
    let host = Live;
    let (add_input, slug) = if let Some((owner, name)) = super::parse_github_slug(input) {
        (format!("{owner}/{name}"), format!("{owner}/{name}"))
    } else {
        let dir = PathBuf::from(input)
            .canonicalize()
            .with_context(|| format!("{input} is not a folder on this computer"))?;
        let slug = origin_slug_of(&dir)
            .with_context(|| format!("{} has no GitHub `origin` remote", dir.display()))?;
        let yml = dir.join(".orch.yml");
        if !yml.exists() && !dir.join(".orchestrator.yml").exists() {
            fs::write(&yml, format!("gh:\n  repo: \"{slug}\"\n"))
                .with_context(|| format!("writing {}", yml.display()))?;
        }
        (dir.to_string_lossy().to_string(), slug)
    };

    let existed = host.config_exists();
    let project_path = host.add_project(&add_input)?;
    if !existed {
        let ready: Vec<String> = DEFAULT_AGENTS
            .iter()
            .filter(|name| host.agent_status(name) == AgentPresence::Ready)
            .map(|name| (*name).to_string())
            .collect();
        host.write_config(&render_minimal_config(
            &project_path.to_string_lossy(),
            &ready,
        ))?;
    }
    Ok(slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::fs;

    struct MockHost {
        gh: GhStatus,
        agents: HashMap<&'static str, AgentPresence>,
        origin: Option<String>,
        prompt: String,
        config: PathBuf,
        local: bool,
        project_path: PathBuf,
        added: RefCell<Option<String>>,
        wrote: Cell<bool>,
        started: Cell<bool>,
        yml: Cell<bool>,
        out: RefCell<String>,
        start_err: Option<String>,
    }

    impl MockHost {
        fn new(dir: &Path) -> Self {
            Self {
                gh: GhStatus::Ready,
                agents: HashMap::from([("claude", AgentPresence::Ready)]),
                origin: None,
                prompt: String::new(),
                config: dir.join("config.yml"),
                local: false,
                project_path: dir.join("project"),
                added: RefCell::new(None),
                wrote: Cell::new(false),
                started: Cell::new(false),
                yml: Cell::new(false),
                out: RefCell::new(String::new()),
                start_err: None,
            }
        }

        fn output(&self) -> String {
            self.out.borrow().clone()
        }
    }

    impl Host for MockHost {
        fn gh_status(&self) -> GhStatus {
            self.gh
        }

        fn agent_status(&self, name: &str) -> AgentPresence {
            self.agents
                .get(name)
                .copied()
                .unwrap_or(AgentPresence::Missing)
        }

        fn git_origin_slug(&self) -> Option<String> {
            self.origin.clone()
        }

        fn prompt_line(&self, prompt: &str) -> anyhow::Result<String> {
            self.out.borrow_mut().push_str(prompt);
            self.out.borrow_mut().push('\n');
            Ok(self.prompt.clone())
        }

        fn config_exists(&self) -> bool {
            self.config.exists()
        }

        fn config_path(&self) -> PathBuf {
            self.config.clone()
        }

        fn write_config(&self, contents: &str) -> anyhow::Result<()> {
            self.wrote.set(true);
            fs::write(&self.config, contents)
                .with_context(|| format!("writing {}", self.config.display()))
        }

        fn local_checkout_of(&self, _slug: &str) -> bool {
            self.local
        }

        fn ensure_local_project_yml(&self, _slug: &str) -> anyhow::Result<()> {
            self.yml.set(true);
            Ok(())
        }

        fn add_project(&self, input: &str) -> anyhow::Result<PathBuf> {
            self.added.replace(Some(input.to_string()));
            Ok(self.project_path.clone())
        }

        fn start_service(&self) -> anyhow::Result<()> {
            if let Some(err) = &self.start_err {
                anyhow::bail!("{err}");
            }
            self.started.set(true);
            Ok(())
        }

        fn print(&self, line: &str) {
            self.out.borrow_mut().push_str(line);
            self.out.borrow_mut().push('\n');
        }
    }

    #[test]
    fn render_omits_agents_when_they_match_defaults() {
        let ready: Vec<String> = DEFAULT_AGENTS.iter().map(ToString::to_string).collect();
        let yaml = render_minimal_config("/tmp/app", &ready);
        assert_eq!(yaml, "projects:\n  - /tmp/app\n");
        assert!(!yaml.contains("agents:"));
        assert!(!yaml.contains("router:"));
    }

    #[test]
    fn render_lists_only_ready_agents_and_router_overrides() {
        let yaml = render_minimal_config("/tmp/app", &["grok".to_string()]);
        assert_eq!(
            yaml,
            "projects:\n  - /tmp/app\n\nagents:\n  - grok\n\nrouter:\n  agent: grok\n  fallback_executor: grok\n"
        );
    }

    #[test]
    fn render_keeps_claude_router_and_overrides_fallback() {
        let yaml = render_minimal_config("/tmp/app", &["claude".to_string()]);
        assert_eq!(
            yaml,
            "projects:\n  - /tmp/app\n\nagents:\n  - claude\n\nrouter:\n  fallback_executor: claude\n"
        );
    }

    #[test]
    fn render_omits_router_when_claude_and_codex_are_ready() {
        let yaml = render_minimal_config("/tmp/app", &["claude".to_string(), "codex".to_string()]);
        assert!(yaml.contains("agents:"));
        assert!(yaml.contains("  - claude"));
        assert!(yaml.contains("  - codex"));
        assert!(!yaml.contains("router:"));
    }

    #[test]
    fn render_quotes_paths_that_need_it() {
        let yaml = render_minimal_config("/tmp/my:app", &["claude".to_string()]);
        assert!(yaml.contains("  - \"/tmp/my:app\""), "{yaml}");
    }

    #[test]
    fn logged_in_output_rejects_not_logged_in_phrase() {
        assert!(output_says_logged_in(true, "Logged in as ada", ""));
        assert!(!output_says_logged_in(
            true,
            "Not logged in. Run grok login.",
            ""
        ));
        assert!(!output_says_logged_in(false, "Logged in as ada", ""));
        assert!(!output_says_logged_in(true, "", "please log in"));
    }

    #[test]
    fn init_writes_minimal_config_starts_service_and_prints_plan() {
        let dir = tempfile::tempdir().unwrap();
        let host = MockHost::new(dir.path());
        run_with(&host, Some("owner/app".into())).unwrap();
        assert_eq!(host.added.borrow().as_deref(), Some("owner/app"));
        assert!(host.wrote.get());
        assert!(host.started.get());
        let yaml = fs::read_to_string(host.config_path()).unwrap();
        assert_eq!(
            yaml,
            format!(
                "projects:\n  - {}\n\nagents:\n  - claude\n\nrouter:\n  fallback_executor: claude\n",
                host.project_path.display()
            )
        );
        assert!(!yaml.contains("mode:"));
        assert!(!yaml.contains("models:"));
        assert!(!yaml.contains("tick_interval"));
        let out = host.output();
        assert!(out.contains("Using claude"), "{out}");
        assert!(out.contains("Next: orch plan <file>"), "{out}");
        assert!(out.contains("Started the orch service"), "{out}");
    }

    #[test]
    fn init_registers_local_checkout_and_writes_project_yml() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = MockHost::new(dir.path());
        host.local = true;
        run_with(&host, Some("owner/app".into())).unwrap();
        assert_eq!(host.added.borrow().as_deref(), Some("."));
        assert!(host.yml.get());
    }

    #[test]
    fn init_uses_origin_when_prompt_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = MockHost::new(dir.path());
        host.origin = Some("owner/from-git".into());
        host.prompt = String::new();
        run_with(&host, None).unwrap();
        assert_eq!(host.added.borrow().as_deref(), Some("owner/from-git"));
        assert!(host.output().contains("Repository [owner/from-git]:"));
    }

    #[test]
    fn init_prompts_for_repo_when_no_flag_or_origin() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = MockHost::new(dir.path());
        host.prompt = "acme/widgets".into();
        run_with(&host, None).unwrap();
        assert_eq!(host.added.borrow().as_deref(), Some("acme/widgets"));
        assert!(host.output().contains("Repository (owner/repo):"));
    }

    #[test]
    fn init_leaves_an_existing_config_alone() {
        let dir = tempfile::tempdir().unwrap();
        let host = MockHost::new(dir.path());
        fs::write(&host.config, "engine:\n  tick_interval: 5\n").unwrap();
        run_with(&host, Some("owner/app".into())).unwrap();
        assert!(!host.wrote.get());
        assert_eq!(
            fs::read_to_string(&host.config).unwrap(),
            "engine:\n  tick_interval: 5\n"
        );
        assert!(host.started.get());
        assert!(host.output().contains("Next: orch plan <file>"));
    }

    #[test]
    fn init_fails_without_gh() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = MockHost::new(dir.path());
        host.gh = GhStatus::Missing;
        let err = run_with(&host, Some("owner/app".into())).unwrap_err();
        assert!(err.to_string().contains("gh is not on PATH"), "{err}");
        assert!(!host.started.get());
        assert!(host.added.borrow().is_none());
    }

    #[test]
    fn init_fails_when_gh_is_logged_out() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = MockHost::new(dir.path());
        host.gh = GhStatus::LoggedOut;
        let err = run_with(&host, Some("owner/app".into())).unwrap_err();
        assert!(err.to_string().contains("gh auth login"), "{err}");
    }

    #[test]
    fn init_fails_without_a_logged_in_agent() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = MockHost::new(dir.path());
        host.agents = HashMap::from([
            ("claude", AgentPresence::LoggedOut),
            ("codex", AgentPresence::Missing),
        ]);
        let err = run_with(&host, Some("owner/app".into())).unwrap_err();
        assert!(
            err.to_string().contains("no agent CLI is logged in"),
            "{err}"
        );
        assert!(host.output().contains("claude     not logged in"));
    }

    #[test]
    fn init_rejects_a_non_slug_repo() {
        let dir = tempfile::tempdir().unwrap();
        let host = MockHost::new(dir.path());
        let err = run_with(&host, Some("/tmp/app".into())).unwrap_err();
        assert!(err.to_string().contains("OWNER/REPO"), "{err}");
    }

    #[test]
    fn init_accepts_a_github_url() {
        let dir = tempfile::tempdir().unwrap();
        let host = MockHost::new(dir.path());
        run_with(&host, Some("https://github.com/owner/app".into())).unwrap();
        assert_eq!(host.added.borrow().as_deref(), Some("owner/app"));
    }

    #[test]
    fn login_status_commands_only_for_clis_that_have_them() {
        assert_eq!(
            login_args("claude"),
            Some(&["auth", "status", "--text"][..])
        );
        assert_eq!(login_args("codex"), Some(&["login", "status"][..]));
        assert!(login_args("grok").is_none());
        assert!(login_args("opencode").is_none());
        assert!(login_args("kimi").is_none());
        assert!(login_args("minimax").is_none());
        assert!(login_args("glm").is_none());
    }

    #[test]
    fn credential_paths_use_auth_files_not_settings() {
        let home = Path::new("/home/ada");
        let grok = credential_paths_in(home, "grok");
        assert_eq!(grok, vec![PathBuf::from("/home/ada/.grok/auth.json")]);
        let claude = credential_paths_in(home, "claude");
        assert_eq!(
            claude,
            vec![PathBuf::from("/home/ada/.claude/.credentials.json")]
        );
        assert!(!claude.iter().any(|path| path.ends_with(".claude.json")));
        let codex = credential_paths_in(home, "codex");
        assert_eq!(codex, vec![PathBuf::from("/home/ada/.codex/auth.json")]);
    }

    #[test]
    fn init_requires_a_repo_when_nothing_is_known() {
        let dir = tempfile::tempdir().unwrap();
        let host = MockHost::new(dir.path());
        let err = run_with(&host, None).unwrap_err();
        assert!(err.to_string().contains("--repo OWNER/REPO"), "{err}");
    }

    #[test]
    fn init_reports_a_service_start_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = MockHost::new(dir.path());
        host.start_err = Some("brew services start orch failed".into());
        let err = run_with(&host, Some("owner/app".into())).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("could not start the service"), "{msg}");
        assert!(msg.contains("orch service start"), "{msg}");
        assert!(host.wrote.get());
    }
}
