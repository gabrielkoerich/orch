//! The OS sandbox around a task or review agent, when `workflow.sandbox` is `os`. The policy and
//! why it is shaped this way are in docs/plans/2026-10-28-agent-sandboxes.md

use super::agent::AgentInvocation;
use sandbox_jail::{known, Backend, Policy, Sandbox};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Files in the home that run later as the operator, so an agent may never write them
const STARTUP: &[&str] = &[
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".bashrc",
    ".bash_profile",
    ".profile",
    ".gitconfig",
    ".config/git",
    ".local/bin",
    "bin",
    "Library/LaunchAgents",
];

/// Whether the project runs its agents in the sandbox: `workflow.sandbox: os` in its `.orch.yml`
/// or the global config. `false` or no setting leaves it off
pub fn enabled(project_dir: &Path) -> bool {
    crate::config::get_for_project(project_dir, "workflow.sandbox").is_ok_and(|v| v.trim() == "os")
}

/// The program and arguments that go before the agent command, or why the sandbox cannot start
pub async fn argv(inv: &AgentInvocation) -> anyhow::Result<Vec<OsString>> {
    let backend = Backend::detect()?;
    let mut git_dirs = vec![];

    for arg in ["--git-dir", "--git-common-dir"] {
        if let Some(dir) = super::agent::resolve_git_dir_arg(&inv.work_dir, arg).await? {
            git_dirs.push(dir);
        }
    }

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set, so the sandbox cannot hide its keys"))?;
    let mut projects: Vec<PathBuf> = crate::config::get_projects_with_paths()
        .unwrap_or_default()
        .into_iter()
        .map(|(_, dir)| dir)
        .collect();
    projects.push(inv.main_project_dir.clone());

    let run = Run {
        work_dir: inv.work_dir.clone(),
        git_dirs,
        home,
        orch_home: crate::home::orch_home()?,
        projects,
        internal: inv.task_id.starts_with("internal:"),
        seatbelt: backend == Backend::Seatbelt,
    };
    let sandbox = Sandbox::new(backend, policy(&run))?;
    Ok(sandbox.argv().to_vec())
}

/// What the policy of one run depends on
struct Run {
    work_dir: PathBuf,
    git_dirs: Vec<PathBuf>,
    home: PathBuf,
    orch_home: PathBuf,
    projects: Vec<PathBuf>,
    internal: bool,
    seatbelt: bool,
}

fn policy(run: &Run) -> Policy {
    let mut p = Policy::default();
    // The home is writable for agent state and every language's caches, minus what runs later
    p.writable.push(run.work_dir.clone());
    p.writable.extend(run.git_dirs.iter().cloned());
    p.writable.extend([std::env::temp_dir(), run.home.clone()]);

    if run.seatbelt {
        p.writable.push("/private/tmp".into());
    }

    // A hook or config written to the git dir runs outside the sandbox the next time the operator runs git
    for dir in &run.git_dirs {
        p.read_only.extend([dir.join("hooks"), dir.join("config")]);
    }

    p.read_only.push(run.work_dir.join(".orch.yml"));
    p.read_only
        .extend(STARTUP.iter().map(|path| run.home.join(path)));

    let mut keep: Vec<PathBuf> = vec![run.work_dir.clone()];
    keep.extend(run.git_dirs.iter().cloned());

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
    p.hidden
        .extend([".gnupg", "Library/Keychains"].map(|path| run.home.join(path)));
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
            "home/proj/src",
        ] {
            fs::create_dir_all(r.join(dir)).unwrap();
        }
        for file in [
            "home/.orch/config.yml",
            "home/.orch/orch.db",
            "home/proj/README.md",
        ] {
            fs::write(r.join(file), "").unwrap();
        }

        let run = Run {
            work_dir: orch.join("worktrees/proj/mine"),
            git_dirs: vec![
                home.join("proj/.git/worktrees/mine"),
                home.join("proj/.git"),
            ],
            home: home.clone(),
            orch_home: orch,
            projects: vec![home.join("proj")],
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

    #[test]
    fn a_worktree_that_is_the_main_checkout_stays_writable() {
        let (_root, mut run) = fixture(false);
        run.work_dir = run.home.join("proj");
        let p = policy(&run);
        assert!(!p.read_only.contains(&run.work_dir));
        assert!(!p.read_only.contains(&run.work_dir.join("src")));
    }
}
