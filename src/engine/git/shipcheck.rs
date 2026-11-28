//! Whether a repository tests through shipcheck, and installing its hooks and rule

use std::collections::HashMap;
use std::env;
use std::io::ErrorKind;
#[cfg(unix)]
use std::path::Path;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;
use tokio::fs;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::OnceCell;
use tokio::time;

/// Whether the agent tests through shipcheck: the repo opts in and the shipcheck on PATH
/// has `commit`. An older shipcheck gets the plain workflow step
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shipcheck {
    Absent,
    NotInstalled,
    Outdated,
    Available,
}

impl Shipcheck {
    pub async fn detect(work_dir: &Path) -> Self {
        static SUPPORTS_COMMIT: OnceCell<Option<bool>> = OnceCell::const_new();
        let installed = SUPPORTS_COMMIT
            .get_or_init(|| async {
                let bin = which::which("shipcheck").ok()?;
                Some(supports(&bin, "commit").await)
            })
            .await;
        Self::classify(work_dir.join("shipcheck.toml").exists(), *installed)
    }

    fn classify(has_config: bool, supports_commit: Option<bool>) -> Self {
        match (has_config, supports_commit) {
            (false, _) => Shipcheck::Absent,
            (true, None) => Shipcheck::NotInstalled,
            (true, Some(false)) => Shipcheck::Outdated,
            (true, Some(true)) => Shipcheck::Available,
        }
    }

    /// What the prompt offers. Inside the OS sandbox shipcheck hands its runs to its daemon, and
    /// without that the agent runs the checks itself and the push hook runs shipcheck outside
    #[must_use]
    pub fn for_prompt(self, sandboxed: bool, hands_off: bool) -> Self {
        if sandboxed && !hands_off && self == Shipcheck::Available {
            return Shipcheck::Absent;
        }

        self
    }

    /// [`Self::for_prompt`], asking the installed shipcheck whether its daemon serves this worktree
    pub async fn in_sandbox(self, sandboxed: bool, work_dir: &Path) -> Self {
        let hands_off = sandboxed && self == Shipcheck::Available && hands_off(work_dir).await;
        self.for_prompt(sandboxed, hands_off)
    }

    pub(crate) fn insert_vars(self, vars: &mut HashMap<String, String>) {
        let flags: &[&str] = match self {
            Shipcheck::Available => &["SHIPCHECK"],
            Shipcheck::NotInstalled => &["NO_SHIPCHECK", "SHIPCHECK_MISSING"],
            Shipcheck::Absent | Shipcheck::Outdated => &["NO_SHIPCHECK"],
        };
        for flag in flags {
            vars.insert(flag.to_string(), "1".to_string());
        }
    }
}

async fn run_shipcheck(bin: &Path, work_dir: &Path, args: &[&str]) -> anyhow::Result<Output> {
    let output = Command::new(bin)
        .args(args)
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    Ok(time::timeout(Duration::from_secs(30), output).await??)
}

// `shipcheck handoff` exits 0 when the daemon watches the repository, and an older one has no such command
async fn hands_off(work_dir: &Path) -> bool {
    let Ok(bin) = which::which("shipcheck") else {
        return false;
    };
    run_shipcheck(&bin, work_dir, &["handoff"])
        .await
        .is_ok_and(|out| out.status.success())
}

// `<sub> --help` exits 0 for any word, so read the subcommands the usage line lists
pub(crate) async fn supports(bin: &Path, subcommand: &str) -> bool {
    let Ok(out) = run_shipcheck(bin, &env::temp_dir(), &["--help"]).await else {
        return false;
    };
    out.status.success()
        && String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .any(|word| word == subcommand)
}

/// Runs `shipcheck install` so every agent CLI gets shipcheck's hooks in this worktree.
/// Returns whether install ran. It never fails the task
pub async fn install_shipcheck_hooks(work_dir: &Path, shipcheck: Shipcheck, bin: &Path) -> bool {
    if shipcheck != Shipcheck::Available {
        return false;
    }

    // A shipcheck without `hook` only installs git hooks, and orch skips it
    if !supports(bin, "hook").await {
        return false;
    }

    match run_shipcheck(bin, work_dir, &["install"]).await {
        Ok(out) if out.status.success() => {}
        Ok(out) => tracing::warn!(
            work_dir = %work_dir.display(),
            stderr = %String::from_utf8_lossy(&out.stderr),
            "shipcheck install failed"
        ),
        Err(e) => tracing::warn!(
            work_dir = %work_dir.display(),
            error = %e,
            "shipcheck install failed"
        ),
    }
    true
}

const SHIPCHECK_RULE_PATH: &str = ".codex/rules/orch-shipcheck.rules";

// Only the commands the agent prompt uses, never clean, post, login or daemon
const SHIPCHECK_RULES: &str = r#"prefix_rule(pattern=["shipcheck", "run"], decision="allow")
prefix_rule(pattern=["shipcheck", "commit"], decision="allow")
prefix_rule(pattern=["shipcheck", "status"], decision="allow")
prefix_rule(pattern=["shipcheck", "logs"], decision="allow")
prefix_rule(pattern=["shipcheck", "doctor"], decision="allow")
"#;

// Codex runs a command a rule allows outside its sandbox, so shipcheck reaches Docker and its
// results dir while the agent's own commands still cannot write there
pub(crate) async fn install_shipcheck_rule(work_dir: &Path) -> anyhow::Result<()> {
    // The worktree holds repository content, so never follow a symlink it plants on the path
    let mut dir = work_dir.to_path_buf();
    for part in [".codex", "rules"] {
        dir.push(part);
        match fs::symlink_metadata(&dir).await {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => anyhow::bail!("{} is not a directory", dir.display()),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                fs::create_dir(&dir).await?;
            }
            Err(e) => return Err(e.into()),
        }
    }

    // remove_file drops a symlink itself, and create_new never follows one
    let rule = work_dir.join(SHIPCHECK_RULE_PATH);
    match fs::remove_file(&rule).await {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&rule)
        .await?;
    AsyncWriteExt::write_all(&mut file, SHIPCHECK_RULES.as_bytes()).await?;

    // Keep the rule out of the agent's commits
    let output = Command::new("git")
        .args(["rev-parse", "--git-path", "info/exclude"])
        .current_dir(work_dir)
        .output()
        .await?;
    if !output.status.success() {
        return Ok(());
    }

    let exclude = work_dir.join(String::from_utf8_lossy(&output.stdout).trim());
    let line = format!("/{SHIPCHECK_RULE_PATH}");
    let current = fs::read_to_string(&exclude).await.unwrap_or_default();
    if current.lines().any(|l| l == line) {
        return Ok(());
    }

    if let Some(dir) = exclude.parent() {
        fs::create_dir_all(dir).await?;
    }

    let sep = if current.is_empty() || current.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    fs::write(&exclude, format!("{current}{sep}{line}\n")).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as std_fs;
    use std::fs::Permissions;
    use std::os::unix::fs as unix_fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process;

    fn git_init(dir: &Path) {
        let out = process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    #[tokio::test]
    async fn install_shipcheck_rule_writes_rule_and_keeps_tree_clean() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());

        install_shipcheck_rule(dir.path()).await.unwrap();
        install_shipcheck_rule(dir.path()).await.unwrap();

        let rule = std_fs::read_to_string(dir.path().join(SHIPCHECK_RULE_PATH)).unwrap();
        assert_eq!(rule, SHIPCHECK_RULES);
        for cmd in ["run", "commit", "status", "logs", "doctor"] {
            assert!(
                rule.contains(&format!(r#"["shipcheck", "{cmd}"]"#)),
                "{cmd}"
            );
        }
        assert!(!rule.contains(r#"["shipcheck"]"#));
        for cmd in [
            "clean", "post", "login", "logout", "daemon", "init", "install",
        ] {
            assert!(!rule.contains(&format!(r#""{cmd}""#)), "{cmd}");
        }

        let exclude = std_fs::read_to_string(dir.path().join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches("orch-shipcheck.rules").count(), 1);
        let status = process::Command::new("git")
            .args(["status", "--porcelain", "--untracked-files=all"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(status.stdout.is_empty(), "{status:?}");
    }

    #[tokio::test]
    async fn install_shipcheck_rule_refuses_symlinked_dirs() {
        for link in [".codex", ".codex/rules"] {
            let dir = tempfile::tempdir().unwrap();
            let target = tempfile::tempdir().unwrap();
            git_init(dir.path());
            std_fs::create_dir_all(dir.path().join(".codex")).unwrap();
            if link == ".codex" {
                std_fs::remove_dir(dir.path().join(".codex")).unwrap();
            }
            unix_fs::symlink(target.path(), dir.path().join(link)).unwrap();

            assert!(install_shipcheck_rule(dir.path()).await.is_err(), "{link}");
            assert_eq!(
                std_fs::read_dir(target.path()).unwrap().count(),
                0,
                "{link}"
            );
        }
    }

    #[tokio::test]
    async fn install_shipcheck_rule_replaces_a_symlinked_file_without_following_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let outside = target.path().join("victim");
        std_fs::write(&outside, "keep").unwrap();
        git_init(dir.path());
        std_fs::create_dir_all(dir.path().join(".codex/rules")).unwrap();
        unix_fs::symlink(&outside, dir.path().join(SHIPCHECK_RULE_PATH)).unwrap();

        install_shipcheck_rule(dir.path()).await.unwrap();

        assert_eq!(std_fs::read_to_string(&outside).unwrap(), "keep");
        let meta = std_fs::symlink_metadata(dir.path().join(SHIPCHECK_RULE_PATH)).unwrap();
        assert!(meta.is_file());
    }

    #[test]
    fn shipcheck_classify_needs_config_and_commit() {
        assert_eq!(Shipcheck::classify(false, Some(true)), Shipcheck::Absent);
        assert_eq!(Shipcheck::classify(false, None), Shipcheck::Absent);
        assert_eq!(Shipcheck::classify(true, None), Shipcheck::NotInstalled);
        assert_eq!(Shipcheck::classify(true, Some(false)), Shipcheck::Outdated);
        assert_eq!(Shipcheck::classify(true, Some(true)), Shipcheck::Available);
    }

    #[tokio::test]
    async fn supports_reads_the_usage_line() {
        let new_dir = tempfile::tempdir().unwrap();
        let new = fake_shipcheck(new_dir.path(), true, true);
        assert!(supports(&new, "commit").await);
        assert!(supports(&new, "hook").await);
        let old_dir = tempfile::tempdir().unwrap();
        let old = fake_shipcheck(old_dir.path(), false, false);
        assert!(!supports(&old, "commit").await);
        assert!(!supports(&old, "hook").await);
        let missing = old_dir.path().join("missing");
        assert!(!supports(&missing, "commit").await);
    }

    // Mimics shipcheck, whose `--help` prints usage and exits 0 after any subcommand
    fn fake_shipcheck(dir: &Path, commit: bool, hook: bool) -> PathBuf {
        let mut usage = "usage: shipcheck run [--sha <rev>] | status | logs".to_string();
        if commit {
            usage.push_str(" | commit -m <message>");
        }
        if hook {
            usage.push_str(" | hook");
        }
        usage.push_str(" | install | doctor");
        let bin = dir.join("shipcheck");
        std_fs::write(
            &bin,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = --help ] && {{ echo '{usage}'; exit 0; }}; done\n[ \"$1\" = install ] && touch installed\nexit 0\n"
            ),
        )
        .unwrap();
        std_fs::set_permissions(&bin, Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[tokio::test]
    async fn install_shipcheck_hooks_runs_install_when_supported() {
        let bin_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let bin = fake_shipcheck(bin_dir.path(), true, true);
        assert!(install_shipcheck_hooks(work.path(), Shipcheck::Available, &bin).await);
        assert!(work.path().join("installed").exists());
    }

    #[tokio::test]
    async fn install_shipcheck_hooks_skips_a_shipcheck_without_hook() {
        let bin_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let bin = fake_shipcheck(bin_dir.path(), true, false);
        assert!(!install_shipcheck_hooks(work.path(), Shipcheck::Available, &bin).await);
        assert!(!work.path().join("installed").exists());
    }

    #[tokio::test]
    async fn install_shipcheck_hooks_skips_when_not_available() {
        let bin_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let bin = fake_shipcheck(bin_dir.path(), true, true);
        for s in [
            Shipcheck::Outdated,
            Shipcheck::NotInstalled,
            Shipcheck::Absent,
        ] {
            assert!(!install_shipcheck_hooks(work.path(), s, &bin).await);
        }
        assert!(!work.path().join("installed").exists());
    }
}
