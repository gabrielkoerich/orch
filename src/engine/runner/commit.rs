//! How orch commits. A repository that uses shipcheck commits through `shipcheck commit`, which
//! commits only a tree its checks passed, any other repository through git

use std::path::Path;
use std::process::Output;

use tokio::process::Command;

use super::agent::Shipcheck;
use super::git_ops;
use crate::cmd::CommandErrorContext;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Git,
    Shipcheck,
}

impl Tool {
    /// The same rule the agent prompt follows: shipcheck when the repository opts in and the
    /// installed shipcheck has `commit`, git otherwise
    pub async fn detect(dir: &Path) -> Self {
        // Without the file there is nothing to ask, and asking runs `shipcheck --help`
        if !dir.join("shipcheck.toml").exists() {
            return Tool::Git;
        }

        match Shipcheck::detect(dir).await {
            Shipcheck::Available => Tool::Shipcheck,
            Shipcheck::Absent | Shipcheck::NotInstalled | Shipcheck::Outdated => Tool::Git,
        }
    }
}

/// Whether a commit may skip shipcheck's gate
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// Only a tree shipcheck already passed, for steps during a session
    Tested,
    /// Commit anyway, for the end of a session, so the agent's work is never left uncommitted.
    /// shipcheck's hooks run the checks when the branch is pushed
    Always,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Committed,
    Nothing,
    /// The commit did not happen, and the tree is as it was. shipcheck refuses a tree without a passing run
    Refused {
        by: Tool,
        reason: String,
    },
}

fn identity(agent: &str) -> (String, String) {
    let name = crate::config::get("git.name").unwrap_or_else(|_| format!("{agent}[bot]"));
    let email = crate::config::get("git.email")
        .unwrap_or_else(|_| format!("{agent}[bot]@users.noreply.github.com"));
    (name, email)
}

/// The git commit behind every orch commit: the agent's identity, and never the operator's signing agent
pub async fn git_commit(dir: &Path, message: &str, agent: &str) -> anyhow::Result<Output> {
    let (name, email) = identity(agent);
    Command::new("git")
        .args(["-c", "commit.gpgsign=false", "commit", "-m", message])
        .env("GIT_AUTHOR_NAME", &name)
        .env("GIT_COMMITTER_NAME", &name)
        .env("GIT_AUTHOR_EMAIL", &email)
        .env("GIT_COMMITTER_EMAIL", &email)
        .current_dir(dir)
        .output_with_context()
        .await
}

async fn shipcheck_commit(
    dir: &Path,
    bin: &Path,
    message: &str,
    agent: &str,
) -> anyhow::Result<Outcome> {
    let (name, email) = identity(agent);
    let out = Command::new(bin)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_NAME", &name)
        .env("GIT_COMMITTER_NAME", &name)
        .env("GIT_AUTHOR_EMAIL", &email)
        .env("GIT_COMMITTER_EMAIL", &email)
        .current_dir(dir)
        .output_with_context()
        .await?;
    if out.status.success() {
        return Ok(Outcome::Committed);
    }

    let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Ok(Outcome::Refused {
        by: Tool::Shipcheck,
        reason: format!("shipcheck commit refused, run `shipcheck run` first: {why}"),
    })
}

/// Commit every change in `dir` with the tool the repository uses
pub async fn commit_all(
    dir: &Path,
    message: &str,
    agent: &str,
    when: When,
) -> anyhow::Result<Outcome> {
    let tool = Tool::detect(dir).await;
    let bin = which::which("shipcheck").unwrap_or_else(|_| "shipcheck".into());
    commit_with(dir, tool, &bin, message, agent, when).await
}

async fn commit_with(
    dir: &Path,
    tool: Tool,
    shipcheck: &Path,
    message: &str,
    agent: &str,
    when: When,
) -> anyhow::Result<Outcome> {
    if !git_ops::has_changes(dir).await {
        return Ok(Outcome::Nothing);
    }

    if tool == Tool::Shipcheck {
        let outcome = shipcheck_commit(dir, shipcheck, message, agent).await?;
        let Outcome::Refused { reason, .. } = &outcome else {
            return Ok(outcome);
        };
        if when == When::Tested {
            return Ok(outcome);
        }

        tracing::info!(reason = %reason, "committing untested work through git at the end of the session");
    }

    let add = Command::new("git")
        .args(["add", "-A"])
        .current_dir(dir)
        .output_with_context()
        .await?;
    if !add.status.success() {
        tracing::warn!("git add -A failed");
        return Ok(Outcome::Refused {
            by: Tool::Git,
            reason: String::from_utf8_lossy(&add.stderr).trim().to_string(),
        });
    }

    let out = git_commit(dir, message, agent).await?;
    if out.status.success() {
        return Ok(Outcome::Committed);
    }

    let reason = String::from_utf8_lossy(&out.stderr).trim().to_string();
    unstage(dir, &reason).await?;
    Ok(Outcome::Refused {
        by: Tool::Git,
        reason,
    })
}

/// Leave a failed commit's files unstaged, so the next check sees them and can retry cleanly
async fn unstage(dir: &Path, commit_err: &str) -> anyhow::Result<()> {
    let restore = Command::new("git")
        .args(["restore", "--staged", "."])
        .current_dir(dir)
        .output_with_context()
        .await;
    let restore_err = match restore {
        Ok(o) if o.status.success() => return Ok(()),
        Ok(o) => String::from_utf8_lossy(&o.stderr).trim().to_string(),
        Err(e) => e.to_string(),
    };
    tracing::warn!(err = %restore_err, "git restore --staged failed, falling back to git reset HEAD -- .");
    let reset = Command::new("git")
        .args(["reset", "HEAD", "--", "."])
        .current_dir(dir)
        .output_with_context()
        .await?;
    if !reset.status.success() {
        anyhow::bail!(
            "git commit failed and cleanup failed (git restore --staged + git reset HEAD -- .): commit stderr: {commit_err}; restore: {restore_err}; reset stderr: {}",
            String::from_utf8_lossy(&reset.stderr).trim()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-b", "main"]);
        std::fs::write(dir.path().join("base.txt"), "base").unwrap();
        git(dir.path(), &["add", "base.txt"]);
        git(dir.path(), &["commit", "-m", "base"]);
        dir
    }

    /// A stand-in for shipcheck: `ok` commits like `shipcheck commit`, anything else refuses
    fn fake_shipcheck(dir: &Path, ok: bool) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("shipcheck");
        let body = if ok {
            "#!/bin/sh\ngit add -A && git -c commit.gpgsign=false commit -q -m \"$3\"\n"
        } else {
            "#!/bin/sh\necho 'no passing result for this tree' >&2\nexit 1\n"
        };
        std::fs::write(&bin, body).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[tokio::test]
    async fn git_commits_every_change_and_reports_nothing_on_a_clean_tree() {
        let dir = repo();
        let missing = Path::new("/nonexistent/shipcheck");
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        let out = commit_with(
            dir.path(),
            Tool::Git,
            missing,
            "step",
            "claude",
            When::Tested,
        )
        .await
        .unwrap();
        assert_eq!(out, Outcome::Committed);
        assert!(
            git(dir.path(), &["log", "-1", "--format=%s%n%an"]).starts_with("step\nclaude[bot]")
        );
        let out = commit_with(
            dir.path(),
            Tool::Git,
            missing,
            "again",
            "claude",
            When::Tested,
        )
        .await
        .unwrap();
        assert_eq!(out, Outcome::Nothing);
    }

    #[tokio::test]
    async fn a_shipcheck_refusal_commits_nothing_and_leaves_the_tree() {
        let dir = repo();
        let bins = tempfile::tempdir().unwrap();
        let bin = fake_shipcheck(bins.path(), false);
        let head = git(dir.path(), &["rev-parse", "HEAD"]);
        std::fs::write(dir.path().join("untested.txt"), "x").unwrap();
        let out = commit_with(
            dir.path(),
            Tool::Shipcheck,
            &bin,
            "step",
            "claude",
            When::Tested,
        )
        .await
        .unwrap();
        match out {
            Outcome::Refused { by, reason } => {
                assert_eq!(by, Tool::Shipcheck);
                assert!(reason.contains("shipcheck run"), "{reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), head);
        assert_eq!(
            git(dir.path(), &["status", "--porcelain"]),
            "?? untested.txt\n"
        );
    }

    #[tokio::test]
    async fn an_accepting_shipcheck_commits() {
        let dir = repo();
        let bins = tempfile::tempdir().unwrap();
        let bin = fake_shipcheck(bins.path(), true);
        std::fs::write(dir.path().join("tested.txt"), "x").unwrap();
        let out = commit_with(
            dir.path(),
            Tool::Shipcheck,
            &bin,
            "tested step",
            "claude",
            When::Tested,
        )
        .await
        .unwrap();
        assert_eq!(out, Outcome::Committed);
        assert_eq!(
            git(dir.path(), &["log", "-1", "--format=%s"]).trim(),
            "tested step"
        );
    }

    #[tokio::test]
    async fn the_end_of_a_session_commits_untested_work_through_git() {
        let dir = repo();
        let bins = tempfile::tempdir().unwrap();
        let bin = fake_shipcheck(bins.path(), false);
        std::fs::write(dir.path().join("untested.txt"), "x").unwrap();
        let out = commit_with(
            dir.path(),
            Tool::Shipcheck,
            &bin,
            "final",
            "claude",
            When::Always,
        )
        .await
        .unwrap();
        assert_eq!(out, Outcome::Committed);
        assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
        assert_eq!(
            git(dir.path(), &["log", "-1", "--format=%s"]).trim(),
            "final"
        );
    }
}
