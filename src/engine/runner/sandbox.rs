//! The OS sandbox around a task or review agent, when `workflow.sandbox` is `os`. The policy and
//! why it is shaped this way are in docs/plans/2026-10-28-agent-sandboxes.md

use super::agent::AgentInvocation;
use sandbox_jail::{known, Access, Backend, Policy, Sandbox};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Files and dirs in the home that run later as the operator, read-only even when missing
const STARTUP: &[&str] = &[
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".zlogin",
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".profile",
    ".gitconfig",
    ".local/bin",
    "bin",
    "Library/LaunchAgents",
];

/// The agents' own settings, hooks and instructions, which the operator's sessions load too
const AGENT_CONFIG: &[&str] = &[
    ".claude/settings.json",
    ".claude/settings.local.json",
    ".claude/hooks",
    ".claude/commands",
    ".claude/agents",
    ".claude/skills",
    ".claude/plugins",
    ".claude/CLAUDE.md",
    ".codex/config.toml",
    ".codex/AGENTS.md",
    ".codex/rules",
];

/// Agent state that stays writable inside read-only parts of the home
const AGENT_STATE: &[&str] = &[".claude.json", ".config/opencode"];

/// The tmux, ssh-agent and Docker sockets that would let an agent start a process outside
const SCRATCH: &[&str] = &["/tmp", "/run", "/var/tmp"];

/// macOS services an agent may look up: the basics, and securityd for its keychain login. Not
/// LaunchServices, which `open` uses to start an app outside the sandbox
const KEYCHAIN: &str = "com.apple.SecurityServer";

/// Whether the project runs its agents in the sandbox: `workflow.sandbox: os` in its `.orch.yml`
/// or the global config. `false` or no setting leaves it off
pub fn enabled(project_dir: &Path) -> bool {
    crate::config::get_for_project(project_dir, "workflow.sandbox").is_ok_and(|v| v.trim() == "os")
}

/// Like [`enabled`] from the global config alone, for a run whose project is unknown
pub fn enabled_globally() -> bool {
    crate::config::get_global("workflow.sandbox").is_ok_and(|v| v.trim() == "os")
}

/// The program and arguments that go before the agent command, or why the sandbox cannot start
pub async fn argv(inv: &AgentInvocation) -> anyhow::Result<Vec<OsString>> {
    let backend = Backend::detect()?;
    let resolve = |arg| super::agent::resolve_git_dir_arg(&inv.work_dir, arg);
    let git_dir = resolve("--git-dir").await?;
    let common_dir = resolve("--git-common-dir").await?;
    let head_ref = tokio::process::Command::new("git")
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .current_dir(&inv.work_dir)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

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
        head_ref,
        home,
        orch_home: crate::home::orch_home()?,
        projects,
        path_dirs,
        temp: std::env::temp_dir(),
        internal: inv.task_id.starts_with("internal:"),
        seatbelt: backend == Backend::Seatbelt,
    };
    let sandbox = Sandbox::new(backend, policy(&run))?;
    Ok(sandbox.argv().to_vec())
}

/// What the policy of one run depends on
struct Run {
    work_dir: PathBuf,
    git_dir: Option<PathBuf>,
    common_dir: Option<PathBuf>,
    /// The worktree's branch, `refs/heads/<branch>`
    head_ref: Option<String>,
    home: PathBuf,
    orch_home: PathBuf,
    projects: Vec<PathBuf>,
    path_dirs: Vec<PathBuf>,
    /// This process's temp dir, where the agent may also connect to a socket
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
    // The home is writable for agent state and every language's caches, minus what runs later
    p.writable.push(run.work_dir.clone());
    p.writable.extend(run.git_dirs().cloned());
    p.writable.push(run.home.clone());
    let temp = run.temp.clone();

    if run.seatbelt {
        p.writable.extend([temp.clone(), "/private/tmp".into()]);
        p.mach_services = Access::Only(
            known::BASIC_MACH_SERVICES
                .iter()
                .chain(&[KEYCHAIN])
                .map(|s| s.to_string())
                .collect(),
        );
    } else {
        // bwrap has no socket filter, so the dirs that hold sockets start empty instead
        p.scratch = SCRATCH
            .iter()
            .map(PathBuf::from)
            .filter(|d| d.is_dir())
            .collect();

        if !p.scratch.iter().any(|s| temp.starts_with(s)) {
            p.writable.push(temp.clone());
        }
    }

    // The tmux server runs outside the sandbox, so its socket in /tmp must stay out of reach
    let mut sockets = vec![run.work_dir.clone()];
    if !["/tmp", "/private/tmp"].iter().any(|t| temp.starts_with(t)) {
        sockets.push(temp);
    }
    p.unix_sockets = Access::Only(sockets);

    if let Some(common) = &run.common_dir {
        git_rules(common, run.head_ref.as_deref(), &mut p.read_only);
    }

    if let Some(dir) = &run.git_dir {
        p.read_only.push(dir.join("config.worktree"));
    }

    p.read_only.push(run.work_dir.join(".orch.yml"));
    home_rules(run, &mut p.read_only);

    let mut keep: Vec<PathBuf> = vec![run.work_dir.clone()];
    keep.extend(run.git_dirs().cloned());

    if run.internal {
        keep.extend(["orch.db", "orch.db-wal", "orch.db-shm"].map(|f| run.orch_home.join(f)));
    }

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
    p.own_signals = true;

    // launchctl and setuid binaries start processes outside the sandbox
    if run.seatbelt {
        p.apple_events = false;
        p.no_exec.push(known::LAUNCHCTL.into());
        p.no_exec.extend(known::setuid_binaries().iter().cloned());
    }

    p
}

/**
 * The git common dir is writable for objects and the task's branch. A hook or config written to it
 * runs the next time the operator runs git, and another branch's ref is the operator's work, so
 * those stay read-only. A branch that only `packed-refs` holds gets a read-only loose ref too,
 * since a new loose ref would win over the packed one
 **/
fn git_rules(common: &Path, head_ref: Option<&str>, out: &mut Vec<PathBuf>) {
    let own = head_ref.map(|r| common.join(r));
    out.extend(
        [
            "hooks",
            "config",
            "packed-refs",
            "refs/tags",
            "modules",
            "info",
        ]
        .map(|d| common.join(d)),
    );
    read_only_around(&common.join("refs/heads"), own.as_slice(), out);

    let packed = std::fs::read_to_string(common.join("packed-refs")).unwrap_or_default();
    for name in packed
        .lines()
        .filter_map(|l| l.split_once(' ').map(|(_, r)| r))
    {
        let path = common.join(name);

        if name.starts_with("refs/heads/") && Some(&path) != own.as_ref() && !path.exists() {
            out.push(path);
        }
    }
}

/**
 * The home is writable, so everything in it that runs later as the operator is read-only: the
 * startup files, every dotfile at its top, `~/.config`, the agents' own settings and every `PATH`
 * dir inside it, such as `~/.cargo/bin`. Agent state those cover stays writable
 **/
fn home_rules(run: &Run, out: &mut Vec<PathBuf>) {
    let home = &run.home;
    out.extend(STARTUP.iter().map(|p| home.join(p)));
    out.extend(AGENT_CONFIG.iter().map(|p| home.join(p)));
    out.extend(
        run.path_dirs
            .iter()
            .filter(|d| d.starts_with(home))
            .cloned(),
    );

    let keep: Vec<PathBuf> = AGENT_STATE.iter().map(|p| home.join(p)).collect();
    read_only_around(&home.join(".config"), &keep, out);

    let Ok(entries) = std::fs::read_dir(home) else {
        return;
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_file = entry
            .file_type()
            .is_ok_and(|t| t.is_file() || t.is_symlink());

        // Claude rewrites its state file and the backups beside it
        if is_file && name.starts_with('.') && !name.starts_with(".claude.json") {
            out.push(entry.path());
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
            "home/.config/nvim",
            "home/.config/opencode",
            "home/.cargo/bin",
        ] {
            fs::create_dir_all(r.join(dir)).unwrap();
        }
        for file in [
            "home/.orch/config.yml",
            "home/.orch/orch.db",
            "home/proj/README.md",
            "home/proj/.git/refs/heads/main",
            "home/proj/.git/refs/heads/feat/mine",
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
            head_ref: Some("refs/heads/feat/mine".into()),
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
        ] {
            assert!(
                p.read_only.contains(&git.join(path)),
                "{path} not read-only"
            );
        }
        assert!(!p.read_only.contains(&git.join("refs/heads/feat/mine")));
        assert!(!p.read_only.contains(&git.join("refs/heads/feat")));
        assert!(p.writable.contains(&git));
    }

    #[test]
    fn what_runs_later_in_the_home_is_read_only() {
        let (_root, run) = fixture(false);
        let p = policy(&run);
        let h = &run.home;
        for path in [
            ".zshrc",
            ".config/nvim",
            ".cargo/bin",
            ".claude/settings.json",
        ] {
            assert!(p.read_only.contains(&h.join(path)), "{path} not read-only");
        }
        for path in [".claude.json", ".config/opencode"] {
            assert!(!p.read_only.contains(&h.join(path)), "{path} read-only");
        }
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
        assert!(sockets.contains(&run.work_dir));
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
    fn a_job_task_may_write_the_store() {
        let (_root, run) = fixture(true);
        assert!(!policy(&run)
            .read_only
            .contains(&run.orch_home.join("orch.db")));
    }

    #[test]
    fn keys_are_hidden_and_startup_files_read_only() {
        let (_root, run) = fixture(false);
        let p = policy(&run);
        assert!(p.hidden.contains(&run.home.join(".ssh")));
        assert!(p.hidden.contains(&run.home.join(".gnupg")));
        assert!(!p.hidden.iter().any(|h| h.ends_with(".cargo/config.toml")));
        assert!(p.read_only.contains(&run.home.join(".zshrc")));
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
             && echo cache > {h}/.cache-probe",
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
            head_ref: None,
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
