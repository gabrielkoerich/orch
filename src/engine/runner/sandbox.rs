//! The OS sandbox around a task or review agent, when `workflow.sandbox` is `os`. The policy and
//! why it is shaped this way are in docs/plans/2026-10-28-agent-sandboxes.md

use super::agent::AgentInvocation;
use sandbox_jail::{known, Access, Backend, Policy, Sandbox};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Top-level dirs of the home that stay read-only: what loads or runs later as the operator, the
/// agents' own settings, and keys. Everything at the top of the home that is not a dir is read-only too
const HOME_READ_ONLY: &[&str] = &[
    ".config", ".claude", ".ssh", ".gnupg", "bin", "Library", ".local",
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

/// The tmux, ssh-agent and Docker sockets that would let an agent start a process outside
const SCRATCH: &[&str] = &["/tmp", "/run", "/var/tmp"];

/// macOS services an agent may look up: the basics, and securityd for its keychain login. Not
/// LaunchServices, which `open` uses to start an app outside the sandbox
const KEYCHAIN: &str = "com.apple.SecurityServer";

/// The run's own temp dir, so no socket of the operator's apps in the shared one is in reach
pub fn run_temp_dir(attempt_dir: &Path) -> PathBuf {
    attempt_dir.join("tmp")
}

/// Whether the project runs its agents in the sandbox: `workflow.sandbox: os` in its `.orch.yml`
/// or the global config. `false` or no setting leaves it off
pub fn enabled(project_dir: &Path) -> bool {
    crate::config::get_for_project(project_dir, "workflow.sandbox").is_ok_and(|v| v.trim() == "os")
}

/// Like [`enabled`] from the global config alone, for a run whose project is unknown
pub fn enabled_globally() -> bool {
    crate::config::get_global("workflow.sandbox").is_ok_and(|v| v.trim() == "os")
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

impl std::fmt::Display for SandboxFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the OS sandbox cannot start: {}", self.0)
    }
}

impl std::error::Error for SandboxFailed {}

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

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set, so the sandbox cannot hide its keys"))?;
    let mut projects: Vec<PathBuf> = crate::config::get_projects_with_paths()
        .unwrap_or_default()
        .into_iter()
        .map(|(_, dir)| dir)
        .collect();
    projects.push(inv.main_project_dir.clone());
    let path_dirs = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();

    let run = Run {
        work_dir: inv.work_dir.clone(),
        git_dir,
        common_dir,
        own_ref: (!inv.branch.is_empty()).then(|| format!("refs/heads/{}", inv.branch)),
        home,
        orch_home: crate::home::orch_home()?,
        projects,
        path_dirs,
        temp: temp.to_path_buf(),
        internal: inv.task_id.starts_with("internal:"),
        seatbelt: backend == Backend::Seatbelt,
    };
    let sandbox = Sandbox::new(backend, policy(&run))?;
    Ok(sandbox.argv().to_vec())
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
            let back = std::fs::read_to_string(git.join("gitdir")).unwrap_or_default();
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

    if run.seatbelt {
        p.writable.push("/private/tmp".into());
        p.mach_services = Access::Only(
            known::BASIC_MACH_SERVICES
                .iter()
                .chain(&[KEYCHAIN])
                .map(std::string::ToString::to_string)
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
    p.unix_sockets = Access::Only(vec![run.work_dir.clone(), run.temp.clone()]);

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

    for dir in home_writable(&run.home) {
        find_git_dirs(&dir, GIT_SCAN_DEPTH, &mut p.read_only);
    }

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
        .collect();
    p.hidden.push(run.home.join(".gnupg"));

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

/// How many levels below a writable home dir are searched for repositories
const GIT_SCAN_DEPTH: usize = 4;

// Hooks and config of a repository orch does not know run as the operator on its next git command
fn find_git_dirs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let git = dir.join(".git");

    if git.is_dir() {
        out.extend(["hooks", "config", "info", "modules"].map(|f| git.join(f)));
    }

    if depth == 0 {
        return;
    }

    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
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
    roots.extend(std::env::var_os("TMUX_TMPDIR").map(PathBuf::from));
    roots.push(temp.to_path_buf());

    roots
        .iter()
        .filter_map(|r| std::fs::read_dir(r).ok())
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

/// The home's writable parts: its top-level dirs but [`HOME_READ_ONLY`], and [`HOME_WRITABLE`]
/// inside those. The home itself stays read-only, so no new dotfile appears at its top
fn home_writable(home: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(home)
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
            out.extend(["HEAD", "commondir", "gitdir", "config.worktree"].map(|f| dir.join(f)));
        }
        // The worktree is the main checkout, so its index and HEAD live in the common dir
        _ => read_only_around(&common.join("refs/heads"), &own, out),
    }

    let packed = std::fs::read_to_string(common.join("packed-refs")).unwrap_or_default();
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
    let Ok(entries) = std::fs::read_dir(dir) else {
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
            internal,
            seatbelt: false,
        };
        (root, run)
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
        assert!(p.read_only.contains(&git.join("worktrees/mine/HEAD")));
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
        assert!(!sockets
            .iter()
            .any(|s| s.starts_with("/tmp") || s.starts_with("/private/tmp")));
        let Access::Only(mach) = &p.mach_services else {
            panic!("every Mach service is reachable");
        };
        assert!(mach.iter().any(|s| s == KEYCHAIN));
        assert!(!mach.iter().any(|s| s.contains("launchservices")));
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
    fn a_job_task_may_write_the_store() {
        let (_root, run) = fixture(true);
        assert!(!policy(&run)
            .read_only
            .contains(&run.orch_home.join("orch.db")));
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
            let listener = std::os::unix::net::UnixListener::bind(dir.join("s")).unwrap();
            std::thread::spawn(move || listener.incoming().for_each(drop));
        }
        let git = run.home.join("proj/.git");
        let script = format!(
            "/usr/bin/nc -U {w}/s </dev/null && ! /usr/bin/nc -U {h}/s </dev/null \
             && echo sha > {g}/refs/heads/feat/mine && ! echo sha > {g}/refs/heads/main",
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
    #[ignore]
    fn claude_answers_inside_the_sandbox_with_the_real_home() {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().canonicalize().unwrap();
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let run = Run {
            work_dir: work.clone(),
            git_dir: None,
            common_dir: None,
            own_ref: None,
            home: home.clone(),
            orch_home: home.join(".orch"),
            projects: vec![],
            path_dirs: std::env::split_paths(&std::env::var_os("PATH").unwrap()).collect(),
            temp: std::env::temp_dir(),
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
