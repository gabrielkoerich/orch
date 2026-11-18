//! The OS sandbox around a task or review agent, on unless `workflow.sandbox` is `false`. The policy and
//! why it is shaped this way are in docs/plans/2026-10-28-agent-sandboxes.md

use super::agent::AgentInvocation;
use crate::config;
use crate::home;
use sandbox_jail::{Access, Backend, Policy, Sandbox, known};
use std::env;
use std::error;
use std::ffi::OsString;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::task;

/// Top-level dirs of the home that stay read-only: what loads or runs later as the operator, the
/// agents' own settings, and keys. Everything at the top of the home that is not a dir is read-only too
const HOME_READ_ONLY: &[&str] = &[
    ".config", ".claude", ".ssh", ".gnupg", "bin", "Library", ".local", ".passbox",
];

/// Writable parts of the read-only dirs above, for caches and agent state
const HOME_WRITABLE: &[&str] = &[
    "Library/Caches",
    "Library/Logs",
    ".local/share",
    ".local/state",
];

/// Codex's settings and instructions, which sit beside the state it rewrites
const AGENT_CONFIG: &[&str] = &[".codex/config.toml", ".codex/AGENTS.md", ".codex/rules"];

/// Config inside writable home dirs that runs code on the operator's next build or shell, read-only
/// even when missing. Cargo's `rustc-wrapper` and `runner`, gradle's init scripts, shell frameworks
const HOME_EXEC_CONFIG: &[&str] = &[
    ".cargo/config",
    ".cargo/config.toml",
    ".gradle/init.d",
    ".gradle/init.gradle",
    ".gradle/gradle.properties",
    ".m2/settings.xml",
    ".oh-my-zsh",
    ".zprezto",
];

/// The one socket in the home an agent may connect to: passbox's broker, which asks the operator
/// before it hands out a secret. Its ssh-agent socket beside it stays out of reach
const HOME_SOCKETS: &[&str] = &[".passbox/broker.sock"];

/// State other tools trust, inside the writable `.local/state`, read-only even when missing:
/// shipcheck posts a PR's required checks from its cache and runs what its `trusted` file names
const HOME_TRUSTED_STATE: &[&str] = &[".local/state/shipcheck"];

/// The address and password of opencode's background service, which runs tools outside the sandbox
const OPENCODE_SERVICE: &[&str] = &[
    ".local/state/opencode/service.json",
    ".config/opencode/service.json",
];

/// The tmux, ssh-agent and Docker sockets that would let an agent start a process outside
const SCRATCH: &[&str] = &["/tmp", "/run", "/var/tmp"];

/// macOS services an agent may look up: the basics, and securityd for its keychain login. Not
/// LaunchServices, which `open` uses to start an app outside the sandbox
const KEYCHAIN: &str = "com.apple.SecurityServer";

/// More services a task agent may look up: certificate trust, which `curl` needs for chains the
/// system file lacks, and the read-only LaunchServices database, which `xcodebuild` needs to tell
/// a project from a folder. Neither starts a process
const TASK_MACH_SERVICES: &[&str] = &["com.apple.TrustEvaluationAgent", "com.apple.lsd.mapdb"];

/// The calendar, which scheduled jobs read. A GitHub-issue task has no use for it
const JOB_MACH_SERVICES: &[&str] = &["com.apple.CalendarAgent"];

/// The run's own temp dir, so no socket of the operator's apps in the shared one is in reach
pub fn run_temp_dir(attempt_dir: &Path) -> PathBuf {
    attempt_dir.join("tmp")
}

/// The empty config dir `gh` gets in the sandbox, inside the run's temp dir
pub fn gh_config_dir(temp: &Path) -> PathBuf {
    temp.join("gh")
}

/// Whether the project runs its agents in the sandbox. It is on unless `workflow.sandbox` is
/// `false` in the project's `.orch.yml` or the global config
pub fn enabled(project_dir: &Path) -> bool {
    is_on(config::get_for_project(project_dir, "workflow.sandbox").ok())
}

/// Like [`enabled`] from the global config alone, for a run whose project is unknown
pub fn enabled_globally() -> bool {
    is_on(config::get_global("workflow.sandbox").ok())
}

// Only an explicit `false` turns it off, so a typo in the value cannot run an agent unsandboxed
fn is_on(setting: Option<String>) -> bool {
    setting.is_none_or(|v| v.trim() != "false")
}

/**
 * The program and arguments that go before the agent command, or why the sandbox cannot start.
 * The worktree's `.git` file is the agent's to write, so the common dir comes from the main
 * checkout and the branch from orch's records, and a git dir outside them refuses the run
 **/
pub async fn argv(inv: &AgentInvocation, temp: &Path) -> anyhow::Result<Vec<OsString>> {
    build(inv, temp)
        .await
        .map_err(|e| anyhow::Error::new(SandboxFailed(format!("{e:#}"))))
}

/// The sandbox cannot start. It is the operator's setup, so it blocks the task and counts against no agent
#[derive(Debug)]
pub struct SandboxFailed(pub String);

impl Display for SandboxFailed {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "the OS sandbox cannot start: {}", self.0)
    }
}

impl error::Error for SandboxFailed {}

async fn build(inv: &AgentInvocation, temp: &Path) -> anyhow::Result<Vec<OsString>> {
    let backend = Backend::detect()?;
    let common_dir =
        super::agent::resolve_git_dir_arg(&inv.main_project_dir, "--git-common-dir").await?;
    let git_dir = super::agent::resolve_git_dir_arg(&inv.work_dir, "--git-dir").await?;

    check_git_dirs(
        &inv.work_dir,
        &inv.main_project_dir,
        git_dir.as_deref(),
        common_dir.as_deref(),
    )?;

    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set, so the sandbox cannot hide its keys"))?;
    let mut projects: Vec<PathBuf> = config::get_projects_with_paths()
        .unwrap_or_default()
        .into_iter()
        .map(|(_, dir)| dir)
        .collect();
    projects.push(inv.main_project_dir.clone());
    let path_dirs = env::var_os("PATH")
        .map(|p| env::split_paths(&p).collect())
        .unwrap_or_default();

    let extra = Extra::from_config(&inv.main_project_dir, &home);
    let run = Run {
        work_dir: inv.work_dir.clone(),
        git_dir,
        common_dir,
        own_ref: (!inv.branch.is_empty()).then(|| format!("refs/heads/{}", inv.branch)),
        home,
        orch_home: home::orch_home()?,
        projects,
        path_dirs,
        temp: temp.to_path_buf(),
        extra,
        internal: inv.task_id.starts_with("internal:"),
        seatbelt: backend == Backend::Seatbelt,
    };
    // The policy reads many dirs, so it stays off the async workers
    task::spawn_blocking(move || {
        let sandbox = Sandbox::new(backend, policy(&run))?;
        Ok(sandbox.argv().to_vec())
    })
    .await?
}

/**
 * Refuses a worktree whose git dir the agent could have pointed elsewhere. Only the main checkout
 * has the common dir as its git dir. A linked worktree's git dir is in `<common>/worktrees`, and
 * its `gitdir` file, which the agent cannot write, points back at this worktree
 **/
fn check_git_dirs(
    work_dir: &Path,
    main_project_dir: &Path,
    git_dir: Option<&Path>,
    common_dir: Option<&Path>,
) -> anyhow::Result<()> {
    let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());

    match (git_dir, common_dir) {
        (Some(git), Some(common)) if git == common => anyhow::ensure!(
            real(work_dir) == real(main_project_dir),
            "the worktree {} uses the repository's own git dir {}",
            work_dir.display(),
            common.display()
        ),
        (Some(git), Some(common)) => {
            anyhow::ensure!(
                git.starts_with(common.join("worktrees")),
                "the worktree's git dir {} is outside its repository {}",
                git.display(),
                common.display()
            );
            let back = fs::read_to_string(git.join("gitdir")).unwrap_or_default();
            anyhow::ensure!(
                real(Path::new(back.trim())) == real(&work_dir.join(".git")),
                "the git dir {} belongs to another worktree",
                git.display()
            );
        }
        (None, Some(common)) => anyhow::bail!(
            "the worktree {} has no git dir in {}",
            work_dir.display(),
            common.display()
        ),
        _ => {}
    }

    Ok(())
}

/**
 * Paths added to the policy. `sandbox.write` adds a writable path, from the main checkout's
 * `.orch.yml` and the global config. `sandbox.read`, which shows a hidden credential path, and
 * `sandbox.sockets`, a unix socket or a dir of them, come from the global config alone: a merged
 * change to `.orch.yml` must not hand later tasks a credential or Docker's socket. No entry lifts
 * a read-only rule, which wins over a writable one
 **/
#[derive(Default)]
struct Extra {
    write: Vec<PathBuf>,
    read: Vec<PathBuf>,
    sockets: Vec<PathBuf>,
}

impl Extra {
    fn from_config(project_dir: &Path, home: &Path) -> Self {
        let paths = |list: Vec<String>| -> Vec<PathBuf> {
            list.iter().filter_map(|text| expand(text, home)).collect()
        };
        let mut write = paths(config::get_list_for_project(project_dir, "sandbox.write"));
        write.retain(|path| {
            let allowed = may_write(path, home);

            if !allowed {
                tracing::warn!(path = %path.display(), "sandbox.write entry refused, it is too wide or holds keys or startup files");
            }

            allowed
        });
        Self {
            write,
            read: paths(config::get_global_list("sandbox.read")),
            sockets: paths(config::get_global_list("sandbox.sockets")),
        }
    }
}

/// Parts of the home no config entry makes writable: keys, the agents' and orch's own files, and
/// what runs at login or from `PATH`
const NEVER_WRITABLE: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".passbox",
    ".claude",
    ".orch",
    ".config/git",
    "bin",
    ".local/bin",
    "library/launchagents",
    "library/keychains",
];

/**
 * Whether a `sandbox.write` entry is narrow enough: not the home, a parent of it or an entry at
 * its top, where the shell's startup files are, and not inside [`NEVER_WRITABLE`], whatever its
 * case, since macOS file systems ignore it. A guard against a mistake: `.orch.yml` is trusted as
 * a whole, its `bash` jobs already run as the operator
 **/
fn may_write(path: &Path, home: &Path) -> bool {
    if home.starts_with(path) || path.components().any(|c| c == Component::ParentDir) {
        return false;
    }

    let Ok(inside) = path.strip_prefix(home) else {
        return true;
    };
    let lower = PathBuf::from(inside.to_string_lossy().to_lowercase());
    inside.components().count() > 1 && !NEVER_WRITABLE.iter().any(|dir| lower.starts_with(dir))
}

/// An absolute path, with `~/` as the home. A relative one names nothing a policy can hold
fn expand(text: &str, home: &Path) -> Option<PathBuf> {
    let path = match text.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(text),
    };
    path.is_absolute().then_some(path)
}

/// Package manager settings in the home, hidden since they can hold a registry token: the file,
/// the variable that points its tool at another one, and whether it holds one setting a line
const PACKAGE_CONFIG: &[(&str, &str, bool)] = &[
    (".npmrc", "NPM_CONFIG_USERCONFIG", true),
    (".config/uv/uv.toml", "UV_CONFIG_FILE", false),
    (".config/pip/pip.conf", "PIP_CONFIG_FILE", false),
];

/// What marks a line of package manager config as a credential
const SECRET_WORDS: &[&str] = &["_auth", "password", "token", "secret", "key", "cert"];

fn holds_secret(line: &str) -> bool {
    let lower = line.to_lowercase();
    // A URL with a user in it, as in https://user:pass@host
    let user_in_url = lower.split("://").skip(1).any(|rest| {
        rest.split(['/', ' ', '"', '\''])
            .next()
            .is_some_and(|host| host.contains('@'))
    });
    user_in_url || SECRET_WORDS.iter().any(|word| lower.contains(word))
}

/**
 * Copies of the operator's package manager settings without their credentials, written under
 * `temp`, as `(variable, file)` for the agent's environment, so an install inside the sandbox
 * follows the operator's rules, such as a minimum package age. `.npmrc` holds one setting a line
 * and loses its credential lines. A TOML or INI file that holds a credential is not copied at
 * all, since a dropped line can break it
 **/
pub fn package_config(home: &Path, temp: &Path) -> Vec<(&'static str, PathBuf)> {
    let dir = temp.join("config");
    let mut found = Vec::new();

    for (file, variable, by_line) in PACKAGE_CONFIG {
        let Ok(text) = fs::read_to_string(home.join(file)) else {
            continue;
        };
        if !by_line && text.lines().any(holds_secret) {
            continue;
        }

        let kept: String = text
            .lines()
            .filter(|line| !holds_secret(line))
            .flat_map(|line| [line, "\n"])
            .collect();
        let copy = dir.join(file.rsplit('/').next().unwrap_or(file));

        if fs::create_dir_all(&dir).is_ok() && fs::write(&copy, kept).is_ok() {
            found.push((*variable, copy));
        }
    }

    found
}

/// What the policy of one run depends on
struct Run {
    work_dir: PathBuf,
    git_dir: Option<PathBuf>,
    common_dir: Option<PathBuf>,
    /// The task's branch, `refs/heads/<branch>`, from orch's records
    own_ref: Option<String>,
    home: PathBuf,
    orch_home: PathBuf,
    projects: Vec<PathBuf>,
    path_dirs: Vec<PathBuf>,
    /// The run's own temp dir, where the agent may also connect to a socket
    temp: PathBuf,
    /// Paths the operator's config adds to the policy
    extra: Extra,
    internal: bool,
    seatbelt: bool,
}

impl Run {
    fn git_dirs(&self) -> impl Iterator<Item = &PathBuf> {
        self.git_dir.iter().chain(&self.common_dir)
    }
}

fn policy(run: &Run) -> Policy {
    let mut p = Policy::default();
    p.writable.push(run.work_dir.clone());
    p.writable.extend(run.git_dirs().cloned());
    p.writable.extend(home_writable(&run.home));
    p.writable.push(run.temp.clone());
    p.writable
        .extend(claude_project_dir(&run.home, &run.work_dir));

    p.writable.extend(run.extra.write.iter().cloned());

    if run.seatbelt {
        p.writable.push("/private/tmp".into());
        let jobs: &[&str] = if run.internal { JOB_MACH_SERVICES } else { &[] };
        p.mach_services = Access::Only(
            known::BASIC_MACH_SERVICES
                .iter()
                .chain(&[KEYCHAIN])
                .chain(TASK_MACH_SERVICES)
                .chain(jobs)
                .map(ToString::to_string)
                .collect(),
        );
    } else {
        // bwrap has no socket filter, so the dirs that hold sockets start empty instead
        p.scratch = SCRATCH
            .iter()
            .map(PathBuf::from)
            .filter(|d| d.is_dir())
            .collect();
    }

    // The tmux server runs outside the sandbox, so its socket in /tmp must stay out of reach
    let mut sockets = vec![run.work_dir.clone(), run.temp.clone()];
    sockets.extend(
        HOME_SOCKETS
            .iter()
            .map(|s| run.home.join(s))
            .filter(|s| s.exists()),
    );
    sockets.extend(run.extra.sockets.iter().cloned());
    p.unix_sockets = Access::Only(sockets);

    if let Some(common) = &run.common_dir {
        git_rules(
            common,
            run.git_dir.as_ref(),
            run.own_ref.as_deref(),
            &mut p.read_only,
        );

        if run.git_dir.as_ref().is_some_and(|g| g != common) {
            p.read_only.push(run.work_dir.join(".git"));
        }
    }

    p.read_only.extend(home_git_dirs(&run.home));

    // /private/tmp stays writable, but the agent must not delete or replace these sockets.
    // bwrap hides those in a scratch dir, and binding one back would expose it
    let hidden = p.scratch.clone();
    p.read_only.extend(
        socket_dirs(&run.temp)
            .into_iter()
            .filter(|d| !hidden.iter().any(|s| d.starts_with(s))),
    );

    p.read_only.push(run.work_dir.join(".orch.yml"));
    p.read_only.extend(
        AGENT_CONFIG
            .iter()
            .chain(HOME_EXEC_CONFIG)
            .chain(HOME_TRUSTED_STATE)
            .map(|path| run.home.join(path)),
    );
    // A tool on PATH inside the home, such as ~/.cargo/bin, runs later as the operator
    p.read_only.extend(
        run.path_dirs
            .iter()
            .filter(|d| d.starts_with(&run.home))
            .cloned(),
    );

    let mut keep: Vec<PathBuf> = vec![run.work_dir.clone(), run.temp.clone()];
    keep.extend(run.git_dirs().cloned());

    if run.internal {
        keep.extend(["orch.db", "orch.db-wal", "orch.db-shm"].map(|f| run.orch_home.join(f)));
    }
    // The daemon's loopback write endpoint takes this token, so reading it is a write to the store
    let write_token = run.orch_home.join("state/cluster.port");

    read_only_around(&run.orch_home, &keep, &mut p.read_only);

    for project in &run.projects {
        read_only_around(project, &keep, &mut p.read_only);
    }

    // The crate's list hides cargo's config too, which holds build settings agents need
    p.hidden = known::credentials(&run.home)
        .into_iter()
        .filter(|path| {
            ![".cargo/config", ".cargo/config.toml"]
                .map(|c| run.home.join(c))
                .contains(path)
        })
        .filter(|path| !run.extra.read.iter().any(|open| path.starts_with(open)))
        .collect();
    p.hidden.push(run.home.join(".gnupg"));
    p.hidden
        .extend(OPENCODE_SERVICE.iter().map(|f| run.home.join(f)));

    // The web UI's token drives the daemon's API, config edits and plan imports among it
    p.hidden.push(run.orch_home.join(UI_TOKEN));

    if !run.internal {
        p.hidden.push(write_token);
    }

    p.own_signals = true;

    // launchctl and setuid binaries start processes outside the sandbox
    if run.seatbelt {
        p.apple_events = false;
        p.no_exec.push(known::LAUNCHCTL.into());
        p.no_exec.extend(known::setuid_binaries().iter().cloned());
    }

    p
}

/// The web UI's port and token. No agent may read it, it would drive the daemon through the UI's API
const UI_TOKEN: &str = "state/ui.port";

/// The agent's own login and settings, which it reads to start. The Security framework reads the
/// login keychain in the calling process, and its access rules still give the agent only its own item
const CHAT_AGENT_HOME: &[&str] = &[".claude", ".claude.json", "Library/Keychains"];

/// Where agents keep sessions and caches, which the control chat writes to resume a conversation
const AGENT_STATE: &[&str] = &[
    ".claude/projects",
    ".claude/todos",
    ".claude/shell-snapshots",
    ".claude/statsig",
    ".codex",
    ".local/share/opencode",
    ".local/state/opencode",
];

/// Other agents' settings, read at start
const AGENT_CONFIG_READ: &[&str] = &[".codex", ".config/opencode", ".grok"];

/// What a chat agent may touch, as opposed to a task agent's [`policy`]
struct ChatRun {
    home: PathBuf,
    orch_home: PathBuf,
    /// The project a project chat reads, none for the control chat
    project: Option<PathBuf>,
    temp: PathBuf,
    path_dirs: Vec<PathBuf>,
    seatbelt: bool,
    /// The control chat keeps agent sessions and acts through orch, so it writes session dirs and
    /// reads the daemon's write token. A project chat does neither
    control: bool,
}

/// The sandbox prefix for a project chat agent. It reads the project and orch's store and config,
/// writes only its temp dir, and cannot read the rest of the home or any credential
pub fn chat_argv(project: &Path, temp: &Path) -> anyhow::Result<Vec<OsString>> {
    chat_sandbox(Some(project), temp, false)
}

/**
 * The sandbox prefix for the control chat, which Telegram and the other channels reach. It reads
 * orch's home and acts through orch, and cannot read the rest of the home or any credential.
 * Inside another Seatbelt sandbox, which cannot nest, it runs in that one
 **/
pub fn control_argv(temp: &Path) -> anyhow::Result<Vec<OsString>> {
    match chat_sandbox(None, temp, true) {
        Err(e)
            if e.downcast_ref::<sandbox_jail::Error>()
                .is_some_and(|e| matches!(e, sandbox_jail::Error::Nested)) =>
        {
            tracing::info!("already inside a sandbox, the control chat runs in it");
            Ok(Vec::new())
        }
        other => other,
    }
}

fn chat_sandbox(
    project: Option<&Path>,
    temp: &Path,
    control: bool,
) -> anyhow::Result<Vec<OsString>> {
    let backend = Backend::detect()?;
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    let run = ChatRun {
        home,
        orch_home: home::orch_home()?,
        project: project.map(Path::to_path_buf),
        temp: temp.to_path_buf(),
        path_dirs: env::var_os("PATH")
            .map(|p| env::split_paths(&p).collect())
            .unwrap_or_default(),
        seatbelt: matches!(backend, Backend::Seatbelt),
        control,
    };
    Ok(Sandbox::new(backend, chat_policy(&run))?.argv().to_vec())
}

fn chat_policy(run: &ChatRun) -> Policy {
    let mut p = Policy::default();
    p.writable.push(run.temp.clone());
    // `orch task list` opens the store, and SQLite writes its -wal and -shm files even to read
    p.writable.extend(
        ["orch.db", "orch.db-wal", "orch.db-shm"]
            .map(|f| run.orch_home.join(f))
            .into_iter()
            .filter(|f| f.exists()),
    );

    if run.seatbelt {
        p.writable.push("/private/tmp".into());
        p.mach_services = Access::Only(
            known::BASIC_MACH_SERVICES
                .iter()
                .chain(&[KEYCHAIN])
                .map(ToString::to_string)
                .collect(),
        );
    } else {
        p.scratch = SCRATCH
            .iter()
            .map(PathBuf::from)
            .filter(|d| d.is_dir())
            .collect();
    }

    p.unix_sockets = Access::Only(vec![run.temp.clone()]);

    if run.control {
        p.writable.extend(
            AGENT_STATE
                .iter()
                .map(|d| run.home.join(d))
                .filter(|d| d.is_dir()),
        );
        // Codex writes its state beside its settings, which stay as the owner wrote them
        p.read_only
            .extend(AGENT_CONFIG.iter().map(|path| run.home.join(path)));
    }

    // The tmux server and launchd's sockets sit in the writable /private/tmp, and an agent that
    // deleted or replaced one would end every session or stand in for the ssh agent
    p.read_only.extend(socket_dirs(&run.temp));

    p.unreadable.push(run.home.clone());
    p.readable.extend(run.project.iter().cloned());
    p.readable.push(run.orch_home.clone());
    p.readable
        .extend(AGENT_CONFIG_READ.iter().map(|f| run.home.join(f)));
    p.readable
        .extend(CHAT_AGENT_HOME.iter().map(|f| run.home.join(f)));
    p.readable.extend(
        run.path_dirs
            .iter()
            .filter(|d| d.starts_with(&run.home))
            .cloned(),
    );

    p.hidden = known::credentials(&run.home);
    p.hidden.push(run.home.join(".gnupg"));
    p.hidden.push(run.orch_home.join(UI_TOKEN));
    // The daemon's loopback write endpoint takes this token, and a project chat only reads
    if !run.control {
        p.hidden.push(run.orch_home.join("state/cluster.port"));
    }

    p.own_signals = true;
    if run.seatbelt {
        p.apple_events = false;
        p.no_exec.push(known::LAUNCHCTL.into());
        p.no_exec.extend(known::setuid_binaries().iter().cloned());
    }

    p
}

/// How many levels below a writable home dir are searched for repositories
const GIT_SCAN_DEPTH: usize = 4;

/// How long a scan of the home for repositories is reused. It takes seconds and every run needs it
/// twice in a row. Kept short, since a repository cloned after the scan is not in it
const GIT_SCAN_TTL: Duration = Duration::from_secs(60);

static GIT_SCAN: Mutex<Option<(Instant, PathBuf, Vec<PathBuf>)>> = Mutex::new(None);

/// The hooks and config of every repository under the home's writable dirs, from a recent scan
fn home_git_dirs(home: &Path) -> Vec<PathBuf> {
    let mut last = GIT_SCAN.lock().unwrap_or_else(PoisonError::into_inner);

    if let Some((at, of, found)) = last.as_ref()
        && of == home
        && at.elapsed() < GIT_SCAN_TTL
    {
        return found.clone();
    }

    let mut found = Vec::new();

    for dir in home_writable(home) {
        find_git_dirs(&dir, GIT_SCAN_DEPTH, &mut found);
    }

    *last = Some((Instant::now(), home.to_path_buf(), found.clone()));
    found
}

// Hooks and config of a repository orch does not know run as the operator on its next git command
fn find_git_dirs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let git = dir.join(".git");

    if git.is_dir() {
        out.extend(["hooks", "config", "info", "modules"].map(|f| git.join(f)));
    }

    if depth == 0 {
        return;
    }

    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name();

        if entry.file_type().is_ok_and(|t| t.is_dir()) && name != ".git" && name != "node_modules" {
            find_git_dirs(&entry.path(), depth - 1, out);
        }
    }
}

/// The tmux and launchd socket dirs in the shared temp dirs, which the tmux server and the
/// ssh-agent keep running outside the sandbox
fn socket_dirs(temp: &Path) -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/private/tmp"), PathBuf::from("/tmp")];
    roots.extend(env::var_os("TMUX_TMPDIR").map(PathBuf::from));
    roots.push(temp.to_path_buf());

    roots
        .iter()
        .filter_map(|r| fs::read_dir(r).ok())
        .flatten()
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("tmux-") || name.starts_with("com.apple.launchd.")
        })
        .map(|e| e.path())
        .collect()
}

/**
 * Where Claude Code keeps the session of a run in this worktree: its transcript, and the tool
 * output too long for the context. Without it a long command output is cut off with no notice.
 * The dir is named after the cwd with every character but letters and digits as `-`
 **/
fn claude_project_dir(home: &Path, work_dir: &Path) -> Option<PathBuf> {
    let projects = home.join(".claude/projects");
    let name: String = work_dir
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let dir = projects.join(name);

    // bwrap binds only a path that exists
    (projects.is_dir() && fs::create_dir_all(&dir).is_ok()).then_some(dir)
}

/// The home's writable parts: its top-level dirs but [`HOME_READ_ONLY`], and [`HOME_WRITABLE`]
/// inside those. The home itself stays read-only, so no new dotfile appears at its top
fn home_writable(home: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(home)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| !HOME_READ_ONLY.contains(&e.file_name().to_string_lossy().as_ref()))
        .map(|e| e.path())
        .collect();
    dirs.extend(
        HOME_WRITABLE
            .iter()
            .map(|d| home.join(d))
            .filter(|d| d.is_dir()),
    );
    dirs
}

/**
 * The git common dir is writable for objects, the task's branch, remote refs and the task's own
 * worktree dir, and read-only around those: hooks and config run the next time the operator runs
 * git, and another branch is the operator's work. Paths git would pick up when they appear are
 * read-only even when missing, and so is a loose ref for a branch only `packed-refs` holds, since
 * it would win over the packed one
 **/
fn git_rules(
    common: &Path,
    git_dir: Option<&PathBuf>,
    own_ref: Option<&str>,
    out: &mut Vec<PathBuf>,
) {
    out.extend(
        [
            "hooks",
            "config",
            "info",
            "modules",
            "packed-refs",
            "refs/tags",
            "commondir",
            "gitdir",
            "config.worktree",
        ]
        .map(|d| common.join(d)),
    );
    let own: Vec<PathBuf> = own_ref
        .map(|r| vec![common.join(r), common.join("logs").join(r)])
        .unwrap_or_default();

    match git_dir {
        Some(dir) if dir != common => {
            let mut keep = vec![
                common.join("objects"),
                common.join("refs/remotes"),
                common.join("logs/refs/remotes"),
                dir.clone(),
            ];
            keep.extend(own.iter().cloned());
            read_only_around(common, &keep, out);
            // HEAD stays writable, a rebase detaches it. Other branches stay read-only whatever it
            // names, and orch pushes the task's branch by name
            out.extend(["commondir", "gitdir", "config.worktree"].map(|f| dir.join(f)));
        }
        // The worktree is the main checkout, so its index and HEAD live in the common dir
        _ => read_only_around(&common.join("refs/heads"), &own, out),
    }

    let packed = fs::read_to_string(common.join("packed-refs")).unwrap_or_default();
    for name in packed
        .lines()
        .filter_map(|l| l.split_once(' ').map(|(_, r)| r))
    {
        let path = common.join(name);

        if name.starts_with("refs/heads/") && !own.contains(&path) && !path.exists() {
            out.push(path);
        }
    }
}

/**
 * Every entry of `dir` read-only but the paths in `keep`. An entry that holds a kept path is
 * walked instead, so `~/.orch` stays read-only around the task's own worktree, and a main checkout
 * around its `.git`. A missing `dir` adds nothing
 **/
fn read_only_around(dir: &Path, keep: &[PathBuf], out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();

        if keep.iter().any(|k| path.starts_with(k)) {
            continue;
        }

        let holds_kept = keep.iter().any(|k| k.starts_with(&path));
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());

        if holds_kept && is_dir {
            read_only_around(&path, keep, out);
        } else if !holds_kept {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(target_os = "macos")]
    use std::os::unix::net::UnixListener;
    #[cfg(target_os = "macos")]
    use std::thread;

    #[test]
    fn only_an_explicit_false_turns_the_sandbox_off() {
        assert!(is_on(None));
        assert!(is_on(Some("os".into())));
        assert!(is_on(Some("flase".into())));
        assert!(!is_on(Some("false".into())));
        assert!(!is_on(Some(" false\n".into())));
    }

    #[tokio::test]
    async fn argv_error_is_tagged_so_spawn_blocks_the_task() {
        let inv = AgentInvocation {
            agent: "claude".to_string(),
            model: None,
            work_dir: PathBuf::from("/nonexistent/work"),
            system_prompt: String::new(),
            agent_message: String::new(),
            task_id: "t".to_string(),
            disallowed_tools: vec![],
            git_author_name: String::new(),
            git_author_email: String::new(),
            output_file: PathBuf::from("/nonexistent/out.json"),
            timeout_seconds: 0,
            repo: "o/r".to_string(),
            attempt: 1,
            sandbox: true,
            main_project_dir: PathBuf::from("/nonexistent/main"),
            branch: String::new(),
        };
        let err = argv(&inv, Path::new("/tmp/orch-test-run"))
            .await
            .expect_err("no checkout, so no sandbox");
        assert!(err.downcast_ref::<SandboxFailed>().is_some());
        assert!(err.to_string().starts_with("the OS sandbox cannot start: "));
    }

    /// A home with an orch home inside, two worktrees and a main checkout, and the run of one task
    fn fixture(internal: bool) -> (tempfile::TempDir, Run) {
        let root = tempfile::tempdir().unwrap();
        let r = root.path().canonicalize().unwrap();
        let (home, orch) = (r.join("home"), r.join("home/.orch"));
        for dir in [
            "home/.orch/worktrees/proj/mine",
            "home/.orch/worktrees/proj/other",
            "home/.orch/state",
            "home/proj/.git/worktrees/mine",
            "home/proj/.git/refs/heads/feat",
            "home/proj/src",
            "home/proj/.git/objects",
            "home/proj/.git/worktrees/other",
            "home/.config/nvim",
            "home/.claude",
            "home/.cargo/bin",
            "home/Library/Caches",
            "home/Library/LaunchAgents",
        ] {
            fs::create_dir_all(r.join(dir)).unwrap();
        }
        for file in [
            "home/.orch/config.yml",
            "home/.orch/orch.db",
            "home/proj/README.md",
            "home/proj/.git/refs/heads/main",
            "home/proj/.git/refs/heads/feat/mine",
            "home/proj/.git/worktrees/mine/HEAD",
            "home/proj/.git/worktrees/mine/gitdir",
            "home/.zshrc",
            "home/.claude.json",
        ] {
            fs::write(r.join(file), "").unwrap();
        }
        fs::write(
            r.join("home/proj/.git/packed-refs"),
            "# pack-refs with: peeled\nabc refs/heads/release\n",
        )
        .unwrap();

        let run = Run {
            work_dir: orch.join("worktrees/proj/mine"),
            git_dir: Some(home.join("proj/.git/worktrees/mine")),
            common_dir: Some(home.join("proj/.git")),
            own_ref: Some("refs/heads/feat/mine".into()),
            home: home.clone(),
            orch_home: orch,
            projects: vec![home.join("proj")],
            path_dirs: vec![home.join(".cargo/bin"), "/usr/bin".into()],
            temp: r.join("tmp"),
            extra: Extra::default(),
            internal,
            seatbelt: false,
        };
        (root, run)
    }

    fn chat_fixture() -> ChatRun {
        ChatRun {
            home: PathBuf::from("/home/u"),
            orch_home: PathBuf::from("/home/u/.orch"),
            project: Some(PathBuf::from("/home/u/code/app")),
            temp: PathBuf::from("/tmp/chat"),
            path_dirs: vec![
                PathBuf::from("/home/u/.cargo/bin"),
                PathBuf::from("/usr/bin"),
            ],
            seatbelt: true,
            control: false,
        }
    }

    #[test]
    fn no_task_can_read_the_web_ui_token() {
        for internal in [false, true] {
            let (_root, run) = fixture(internal);
            let token = run.orch_home.join("state/ui.port");
            assert!(policy(&run).hidden.contains(&token), "internal: {internal}");
        }
    }

    #[test]
    fn no_chat_can_read_the_web_ui_token() {
        let token = PathBuf::from("/home/u/.orch/state/ui.port");
        let mut run = chat_fixture();
        assert!(
            chat_policy(&run).hidden.contains(&token),
            "the project chat"
        );
        run.project = None;
        run.control = true;
        assert!(
            chat_policy(&run).hidden.contains(&token),
            "the control chat"
        );
    }

    #[test]
    fn the_control_chat_reads_no_project_and_may_act_through_orch() {
        let mut run = chat_fixture();
        run.project = None;
        run.control = true;
        let p = chat_policy(&run);
        assert!(
            !p.readable.iter().any(|r| r.starts_with("/home/u/code")),
            "no project folder"
        );
        assert!(
            !p.hidden
                .contains(&PathBuf::from("/home/u/.orch/state/cluster.port")),
            "it acts through the daemon"
        );
        assert!(
            p.hidden.contains(&PathBuf::from("/home/u/.ssh")),
            "credentials stay hidden"
        );
        assert!(
            p.unreadable.contains(&PathBuf::from("/home/u")),
            "the home stays closed"
        );
    }

    #[test]
    fn a_chat_writes_only_its_temp_dir() {
        let p = chat_policy(&chat_fixture());
        assert!(
            p.writable.contains(&PathBuf::from("/tmp/chat")),
            "its temp dir"
        );
        assert!(
            !p.writable.iter().any(|w| w.starts_with("/home/u/code")),
            "never the project"
        );
    }

    #[test]
    fn a_chat_reads_the_project_but_not_the_rest_of_the_home() {
        let p = chat_policy(&chat_fixture());
        assert!(
            p.unreadable.contains(&PathBuf::from("/home/u")),
            "the home is closed"
        );
        assert!(
            p.readable.contains(&PathBuf::from("/home/u/code/app")),
            "the project is open"
        );
        assert!(
            p.readable.contains(&PathBuf::from("/home/u/.claude")),
            "the agent can log in"
        );
        assert!(
            p.readable.contains(&PathBuf::from("/home/u/.cargo/bin")),
            "tools on PATH"
        );
    }

    #[test]
    fn a_chat_cannot_see_credentials_or_the_write_token() {
        let p = chat_policy(&chat_fixture());
        assert!(
            p.hidden.contains(&PathBuf::from("/home/u/.ssh")),
            "ssh keys"
        );
        assert!(
            p.hidden.contains(&PathBuf::from("/home/u/.config/gh")),
            "the gh token"
        );
        assert!(
            p.hidden
                .contains(&PathBuf::from("/home/u/.orch/state/cluster.port")),
            "the daemon's write token"
        );
    }

    #[test]
    fn orch_home_is_read_only_around_the_task_worktree() {
        let (_root, run) = fixture(false);
        let p = policy(&run);
        let o = &run.orch_home;
        for path in [
            o.join("config.yml"),
            o.join("state"),
            o.join("orch.db"),
            o.join("worktrees/proj/other"),
        ] {
            assert!(
                p.read_only.contains(&path),
                "{} not read-only",
                path.display()
            );
        }
        for path in [
            o.join("worktrees"),
            o.join("worktrees/proj"),
            run.work_dir.clone(),
        ] {
            assert!(!p.read_only.contains(&path), "{} read-only", path.display());
        }
        assert!(p.writable.contains(&run.work_dir));
    }

    #[test]
    fn the_main_checkout_is_read_only_but_its_git_dir() {
        let (_root, run) = fixture(false);
        let p = policy(&run);
        let proj = run.home.join("proj");
        assert!(p.read_only.contains(&proj.join("src")));
        assert!(p.read_only.contains(&proj.join("README.md")));
        assert!(!p.read_only.contains(&proj.join(".git")));
        assert!(p.read_only.contains(&proj.join(".git/hooks")));
        assert!(p.read_only.contains(&proj.join(".git/config")));
    }

    #[test]
    fn only_the_task_branch_is_writable_in_the_git_dir() {
        let (_root, run) = fixture(false);
        let p = policy(&run);
        let git = run.home.join("proj/.git");
        for path in [
            "refs/heads/main",
            "refs/heads/release",
            "packed-refs",
            "hooks",
            "config",
            "commondir",
            "gitdir",
            "config.worktree",
        ] {
            assert!(
                p.read_only.contains(&git.join(path)),
                "{path} not read-only"
            );
        }
        assert!(!p.read_only.contains(&git.join("refs/heads/feat/mine")));
        assert!(!p.read_only.contains(&git.join("refs/heads/feat")));
        assert!(!p.read_only.contains(&git.join("objects")));
        assert!(p.read_only.contains(&git.join("worktrees/other")));
        assert!(p.read_only.contains(&git.join("worktrees/mine/gitdir")));
        assert!(!p.read_only.contains(&git.join("worktrees/mine/HEAD")));
        assert!(p.read_only.contains(&run.work_dir.join(".git")));
        assert!(p.writable.contains(&git));
    }

    #[test]
    fn hooks_and_config_of_an_unknown_repository_are_read_only() {
        let (_root, run) = fixture(false);
        let git = run.home.join("code/deep/other/.git");
        fs::create_dir_all(&git).unwrap();
        let p = policy(&run);
        assert!(p.read_only.contains(&git.join("hooks")));
        assert!(p.read_only.contains(&git.join("config")));
        assert!(!p.read_only.contains(&git));
    }

    #[test]
    fn only_the_home_s_cache_dirs_are_writable() {
        let (_root, run) = fixture(false);
        let p = policy(&run);
        let h = &run.home;
        for path in [".cargo", ".orch", "Library/Caches"] {
            assert!(p.writable.contains(&h.join(path)), "{path} not writable");
        }
        for path in ["", ".config", ".claude", "Library"] {
            assert!(!p.writable.contains(&h.join(path)), "{path:?} writable");
        }
        assert!(p.read_only.contains(&h.join(".cargo/bin")));
        assert!(!p.read_only.contains(&PathBuf::from("/usr/bin")));
    }

    #[test]
    fn no_socket_outside_the_worktree_and_no_launch_services() {
        let (_root, mut run) = fixture(false);
        run.seatbelt = true;
        let p = policy(&run);
        let Access::Only(sockets) = &p.unix_sockets else {
            panic!("every unix socket is reachable");
        };
        assert_eq!(sockets, &vec![run.work_dir.clone(), run.temp.clone()]);

        // Only the broker's socket, and the store beside it is not the agent's to write
        fs::create_dir_all(run.home.join(".passbox")).unwrap();
        fs::write(run.home.join(".passbox/broker.sock"), "").unwrap();
        fs::write(run.home.join(".passbox/ssh-agent.sock"), "").unwrap();
        let p = policy(&run);
        let Access::Only(sockets) = &p.unix_sockets else {
            panic!("every unix socket is reachable");
        };
        assert!(sockets.contains(&run.home.join(".passbox/broker.sock")));
        assert!(!sockets.contains(&run.home.join(".passbox/ssh-agent.sock")));
        assert!(!p.writable.contains(&run.home.join(".passbox")));
        assert!(
            !sockets
                .iter()
                .any(|s| s.starts_with("/tmp") || s.starts_with("/private/tmp"))
        );
        let Access::Only(mach) = &p.mach_services else {
            panic!("every Mach service is reachable");
        };
        assert!(mach.iter().any(|s| s == KEYCHAIN));
        assert!(!mach.iter().any(|s| s.contains("launchservices")));
        assert!(!mach.iter().any(|s| s.contains("CalendarAgent")));

        let (_root, mut job) = fixture(true);
        job.seatbelt = true;
        let Access::Only(mach) = policy(&job).mach_services else {
            panic!("every Mach service is reachable");
        };
        assert!(mach.iter().any(|s| s == "com.apple.CalendarAgent"));
    }

    #[test]
    fn code_running_config_in_a_writable_home_dir_is_read_only() {
        let (_root, run) = fixture(false);
        let p = policy(&run);
        assert!(p.writable.contains(&run.home.join(".cargo")));
        assert!(p.read_only.contains(&run.home.join(".cargo/config.toml")));
    }

    #[test]
    fn a_git_dir_the_agent_could_have_pointed_elsewhere_is_refused() {
        let (_root, run) = fixture(false);
        let (work, main) = (&run.work_dir, run.home.join("proj"));
        let (git, common) = (
            run.git_dir.clone().unwrap(),
            run.common_dir.clone().unwrap(),
        );

        fs::write(
            git.join("gitdir"),
            format!("{}\n", work.join(".git").display()),
        )
        .unwrap();
        assert!(check_git_dirs(work, &main, Some(&git), Some(&common)).is_ok());

        // Pointed at the common dir, which only the main checkout uses
        assert!(check_git_dirs(work, &main, Some(&common), Some(&common)).is_err());
        assert!(check_git_dirs(&main, &main, Some(&common), Some(&common)).is_ok());
        // Another worktree's git dir, or none at all
        let other = common.join("worktrees/other");
        fs::write(other.join("gitdir"), "/elsewhere/.git\n").unwrap();
        assert!(check_git_dirs(work, &main, Some(&other), Some(&common)).is_err());
        assert!(check_git_dirs(work, &main, Some(&run.home), Some(&common)).is_err());
        assert!(check_git_dirs(work, &main, None, Some(&common)).is_err());
    }

    #[test]
    fn the_config_adds_paths_and_lifts_no_read_only_rule() {
        let (_root, mut run) = fixture(false);
        let h = run.home.clone();
        run.extra = Extra {
            write: vec![h.join(".config/solana/install")],
            read: vec![h.join(".aws")],
            sockets: vec![h.join(".docker/run")],
        };
        let p = policy(&run);
        assert!(p.writable.contains(&h.join(".config/solana/install")));
        assert!(!p.hidden.contains(&h.join(".aws")));
        assert!(p.hidden.contains(&h.join(".ssh")));
        let Access::Only(sockets) = &p.unix_sockets else {
            panic!("every unix socket is reachable");
        };
        assert!(sockets.contains(&h.join(".docker/run")));
        assert!(p.read_only.contains(&h.join(".cargo/bin")));
    }

    #[test]
    fn a_wide_write_entry_and_one_on_keys_or_startup_files_is_refused() {
        let home = Path::new("/home/u");
        for allowed in [
            "/home/u/.config/solana/install",
            "/home/u/Library/Developer/Xcode",
            "/opt/tool/cache",
        ] {
            assert!(may_write(Path::new(allowed), home), "{allowed}");
        }
        for refused in [
            "/",
            "/home",
            "/home/u",
            "/home/u/.zshrc",
            "/home/u/.config",
            "/home/u/.ssh/authorized_keys",
            "/home/u/.claude/settings.json",
            "/home/u/.local/bin/tool",
            "/home/u/Library/LaunchAgents/x.plist",
            "/home/u/.orch/config.yml",
            "/home/u/.config/../.ssh/x",
            "/home/u/.SSH/authorized_keys",
            "/home/u/.config/git/config",
        ] {
            assert!(!may_write(Path::new(refused), home), "{refused}");
        }
    }

    #[test]
    fn a_config_path_is_absolute_or_under_the_home() {
        let home = Path::new("/home/u");
        assert_eq!(expand("~/.pypirc", home), Some("/home/u/.pypirc".into()));
        assert_eq!(expand("/opt/tool", home), Some("/opt/tool".into()));
        assert_eq!(expand("relative/path", home), None);
        assert_eq!(expand("~other/x", home), None);
    }

    #[test]
    fn package_settings_reach_the_agent_without_their_credentials() {
        let (root, run) = fixture(false);
        let h = &run.home;
        fs::write(
            h.join(".npmrc"),
            "min-release-age=7\n//registry.npmjs.org/:_authToken=npm_abc\nregistry=https://user:pw@r.example/\nsave-exact=true\n",
        )
        .unwrap();
        fs::create_dir_all(h.join(".config/uv")).unwrap();
        fs::write(h.join(".config/uv/uv.toml"), "exclude-newer = \"7 days\"\n").unwrap();
        fs::create_dir_all(h.join(".config/pip")).unwrap();
        fs::write(
            h.join(".config/pip/pip.conf"),
            "[global]\nindex-url = https://u:p@pypi.example/simple\n",
        )
        .unwrap();

        let temp = root.path().join("run-tmp");
        let found = package_config(h, &temp);
        let names: Vec<&str> = found.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["NPM_CONFIG_USERCONFIG", "UV_CONFIG_FILE"]);
        let npmrc = fs::read_to_string(&found[0].1).unwrap();
        assert_eq!(npmrc, "min-release-age=7\nsave-exact=true\n");
        let uv = fs::read_to_string(&found[1].1).unwrap();
        assert_eq!(uv, "exclude-newer = \"7 days\"\n");
        assert!(found.iter().all(|(_, file)| file.starts_with(&temp)));

        // The files themselves stay hidden
        let p = policy(&run);
        assert!(p.hidden.contains(&h.join(".npmrc")));
        assert!(p.hidden.contains(&h.join(".config/uv")));
    }

    #[test]
    fn claude_s_session_dir_of_the_worktree_is_writable() {
        let (_root, run) = fixture(false);
        assert!(claude_project_dir(&run.home, &run.work_dir).is_none());
        fs::create_dir_all(run.home.join(".claude/projects/-other")).unwrap();
        let p = policy(&run);
        let name = run.work_dir.to_string_lossy().replace(['/', '.'], "-");
        let dir = run.home.join(".claude/projects").join(name);
        assert!(dir.is_dir());
        assert!(p.writable.contains(&dir));
        assert!(!p.writable.contains(&run.home.join(".claude/projects")));
        assert!(!p.writable.contains(&run.home.join(".claude")));
    }

    #[test]
    fn shipcheck_s_state_is_read_only_and_opencode_s_service_hidden() {
        let (_root, run) = fixture(false);
        let p = policy(&run);
        let h = &run.home;
        assert!(p.read_only.contains(&h.join(".local/state/shipcheck")));
        assert!(
            p.hidden
                .contains(&h.join(".local/state/opencode/service.json"))
        );
        assert!(p.hidden.contains(&h.join(".config/opencode/service.json")));
    }

    #[test]
    fn a_job_task_may_write_the_store() {
        let (_root, run) = fixture(true);
        assert!(
            !policy(&run)
                .read_only
                .contains(&run.orch_home.join("orch.db"))
        );
    }

    #[test]
    fn only_a_job_task_sees_the_write_endpoint_token() {
        let (_root, run) = fixture(false);
        let token = run.orch_home.join("state/cluster.port");
        assert!(policy(&run).hidden.contains(&token));
        let (_root, run) = fixture(true);
        assert!(!policy(&run).hidden.contains(&token));
    }

    #[test]
    fn keys_are_hidden_and_startup_files_read_only() {
        let (_root, run) = fixture(false);
        let p = policy(&run);
        assert!(p.hidden.contains(&run.home.join(".ssh")));
        assert!(p.hidden.contains(&run.home.join(".gnupg")));
        assert!(!p.hidden.iter().any(|h| h.ends_with(".cargo/config.toml")));
        assert!(p.read_only.contains(&run.work_dir.join(".orch.yml")));
    }

    // A real Seatbelt run of the policy. Skipped where no sandbox starts, such as inside another one
    #[cfg(target_os = "macos")]
    #[test]
    fn the_kernel_enforces_the_policy() {
        let Ok(backend) = Backend::detect() else {
            eprintln!("skipped: no OS sandbox starts here");
            return;
        };
        let (_root, mut run) = fixture(false);
        run.seatbelt = backend == Backend::Seatbelt;
        let other = run.orch_home.join("worktrees/proj/other");
        fs::create_dir_all(run.home.join(".ssh")).unwrap();
        fs::write(run.home.join(".ssh/key"), "k").unwrap();
        let script = format!(
            "echo ok > {w}/f && ! echo x > {o}/config.yml && ! echo x > {other}/f \
             && ! echo x > {w}/.orch.yml && ! echo x >> {h}/.zshrc && ! cat {h}/.ssh/key \
             && ! echo x > {h}/.new-dotfile && ! echo x > {h}/.claude.json \
             && echo cache > {h}/.cargo/probe",
            w = run.work_dir.display(),
            o = run.orch_home.display(),
            other = other.display(),
            h = run.home.display(),
        );
        let out = Sandbox::new(backend, policy(&run))
            .unwrap()
            .command("/bin/sh")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(run.work_dir.join("f").exists());
        assert!(!other.join("f").exists());
    }

    // A socket outside the worktree, such as tmux's, is out of reach, and another branch's ref too
    #[cfg(target_os = "macos")]
    #[test]
    fn the_kernel_keeps_sockets_and_other_branches_out_of_reach() {
        let Ok(backend) = Backend::detect() else {
            eprintln!("skipped: no OS sandbox starts here");
            return;
        };
        let (_root, mut run) = fixture(false);
        run.seatbelt = backend == Backend::Seatbelt;
        for dir in [&run.work_dir, &run.home] {
            let listener = UnixListener::bind(dir.join("s")).unwrap();
            thread::spawn(move || listener.incoming().for_each(drop));
        }
        let git = run.home.join("proj/.git");
        let script = format!(
            "/usr/bin/nc -U {w}/s </dev/null && ! /usr/bin/nc -U {h}/s </dev/null \
             && echo sha > {g}/refs/heads/feat/mine && ! echo sha > {g}/refs/heads/main \
             && echo sha > {g}/worktrees/mine/HEAD && ! echo x > {g}/worktrees/mine/gitdir",
            w = run.work_dir.display(),
            h = run.home.display(),
            g = git.display(),
        );
        let out = Sandbox::new(backend, policy(&run))
            .unwrap()
            .command("/bin/sh")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(fs::read_to_string(git.join("refs/heads/main")).unwrap(), "");
    }

    // A missing commondir in the common dir would redirect git to a dir the agent controls
    #[cfg(target_os = "macos")]
    #[test]
    fn the_kernel_refuses_a_new_commondir_in_the_common_dir() {
        let Ok(backend) = Backend::detect() else {
            eprintln!("skipped: no OS sandbox starts here");
            return;
        };
        let (_root, mut run) = fixture(false);
        run.seatbelt = backend == Backend::Seatbelt;
        let git = run.home.join("proj/.git");
        assert!(!git.join("commondir").exists());
        let script = format!(
            "! echo {w} > {g}/commondir",
            w = run.work_dir.display(),
            g = git.display()
        );
        let out = Sandbox::new(backend, policy(&run))
            .unwrap()
            .command("/bin/sh")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!git.join("commondir").exists());
    }

    // Removing or replacing tmux's socket file is refused
    #[cfg(target_os = "macos")]
    #[test]
    fn the_kernel_keeps_the_tmux_socket_in_the_temp_dir_in_place() {
        let Ok(backend) = Backend::detect() else {
            eprintln!("skipped: no OS sandbox starts here");
            return;
        };
        let (_root, mut run) = fixture(false);
        run.seatbelt = backend == Backend::Seatbelt;
        let dir = tempfile::Builder::new()
            .prefix("tmux-")
            .tempdir_in(&run.home)
            .unwrap();
        run.temp = run.home.clone();
        let sock = dir.path().join("default");
        fs::write(&sock, "").unwrap();
        let script = format!(
            "! rm {s} && ! mv {s} {s}2 && ! echo x > {d}/new",
            s = sock.display(),
            d = dir.path().display(),
        );
        let out = Sandbox::new(backend, policy(&run))
            .unwrap()
            .command("/bin/sh")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(sock.exists());
    }

    // A logged-in claude answers under the policy with the real home: `--run-ignored only claude`
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "needs a logged-in claude on the host"]
    fn claude_answers_inside_the_sandbox_with_the_real_home() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().canonicalize().unwrap();
        let home = PathBuf::from(env::var_os("HOME").unwrap());
        let run = Run {
            work_dir: work.clone(),
            git_dir: None,
            common_dir: None,
            own_ref: None,
            home: home.clone(),
            orch_home: home.join(".orch"),
            projects: vec![],
            path_dirs: env::split_paths(&env::var_os("PATH").unwrap()).collect(),
            temp: env::temp_dir(),
            extra: Extra::default(),
            internal: false,
            seatbelt: true,
        };
        let out = Sandbox::new(Backend::Seatbelt, policy(&run))
            .unwrap()
            .command("/bin/sh")
            .args(["-c", "claude -p --model haiku 'Reply with the word ok'"])
            .current_dir(&work)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
        assert!(
            text.contains("ok"),
            "{text}{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn a_worktree_that_is_the_main_checkout_stays_writable() {
        let (_root, mut run) = fixture(false);
        run.work_dir = run.home.join("proj");
        let p = policy(&run);
        assert!(!p.read_only.contains(&run.work_dir));
        assert!(!p.read_only.contains(&run.work_dir.join("src")));
    }
}
