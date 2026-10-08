//! Copy-on-write task worktrees cloned from a per-project template, see docs/content/cow-worktrees.md

use super::worktree::resolve_branch_start_point;
use crate::cmd::CommandErrorContext;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use tokio::process::Command;

// One lock per template, so a clone never sees a half-updated template and projects do not block each other
static TEMPLATE_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn template_lock(template: &Path) -> Arc<tokio::sync::Mutex<()>> {
    TEMPLATE_LOCKS
        .lock()
        .unwrap()
        .entry(template.to_path_buf())
        .or_default()
        .clone()
}

// Clone support per (template dir, worktree dir), probed once per process
static PROBES: LazyLock<Mutex<HashMap<(PathBuf, PathBuf), bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// Never cloned, the worktree gets its own gitdir
const SKIP: &[&str] = &[".git"];

fn clone_flags() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        // clonefile(2)
        &["-cRp"]
    } else {
        // FICLONE ioctl, fails instead of copying when the volume cannot clone
        &["-a", "--reflink=always"]
    }
}

async fn clone_path(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let out = Command::new("cp")
        .args(clone_flags())
        .arg(src)
        .arg(dst)
        .output_with_context()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "cp {}: {}",
            src.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

async fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output_with_context()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// Try to clone one small file from `src_dir` into `dst_dir`
async fn volume_can_clone(src_dir: &Path, dst_dir: &Path) -> bool {
    let key = (src_dir.to_path_buf(), dst_dir.to_path_buf());
    if let Some(known) = PROBES.lock().unwrap().get(&key) {
        return *known;
    }
    let src = src_dir.join(".cow-probe-src");
    let dst = dst_dir.join(".cow-probe-dst");
    let _ = tokio::fs::remove_file(&dst).await;
    let ok = tokio::fs::write(&src, b"x").await.is_ok() && clone_path(&src, &dst).await.is_ok();
    let _ = tokio::fs::remove_file(&src).await;
    let _ = tokio::fs::remove_file(&dst).await;
    PROBES.lock().unwrap().insert(key, ok);
    ok
}

// Create the template checkout, or move it to the current default branch tip
async fn refresh_template(
    main_dir: &Path,
    template: &Path,
    default_branch: &str,
) -> anyhow::Result<()> {
    let main = main_dir.to_string_lossy();
    let start = resolve_branch_start_point(&main, default_branch, default_branch).await;
    if template.join(".git").exists()
        && git(template, &["checkout", "--detach", "--force", &start])
            .await
            .is_ok()
    {
        return Ok(());
    }
    let _ = tokio::fs::remove_dir_all(template).await;
    let _ = git(main_dir, &["worktree", "prune"]).await;
    git(
        main_dir,
        &[
            "worktree",
            "add",
            "--detach",
            &template.to_string_lossy(),
            &start,
        ],
    )
    .await?;
    Ok(())
}

async fn populate(
    main_dir: &Path,
    template: &Path,
    wt_dir: &Path,
    branch: &str,
) -> anyhow::Result<()> {
    // --no-checkout creates the gitdir entry and the `.git` file, so git state stays per worktree
    git(
        main_dir,
        &[
            "worktree",
            "add",
            "--no-checkout",
            &wt_dir.to_string_lossy(),
            branch,
        ],
    )
    .await?;

    let mut entries = tokio::fs::read_dir(template).await?;
    while let Some(entry) = entries.next_entry().await? {
        if SKIP.contains(&entry.file_name().to_string_lossy().as_ref()) {
            continue;
        }
        clone_path(&entry.path(), wt_dir).await?;
    }

    // The template index describes the cloned files. Refresh its stat data for the new inodes,
    // then reset only the files where the task branch differs from the template commit
    let tpl_git = git(template, &["rev-parse", "--absolute-git-dir"]).await?;
    let wt_git = git(wt_dir, &["rev-parse", "--absolute-git-dir"]).await?;
    tokio::fs::copy(
        Path::new(&tpl_git).join("index"),
        Path::new(&wt_git).join("index"),
    )
    .await?;
    let _ = git(wt_dir, &["update-index", "-q", "--refresh"]).await;
    git(wt_dir, &["reset", "--hard", "-q"]).await?;
    Ok(())
}

// Create `wt_dir` as a worktree of `branch` by cloning the project template.
// On error nothing is left behind and the caller falls back to `git worktree add`
pub async fn create_worktree(
    main_dir: &Path,
    project: &str,
    wt_dir: &Path,
    branch: &str,
    default_branch: &str,
) -> anyhow::Result<()> {
    if std::env::var("ORCH_COW_WORKTREES").is_ok_and(|v| v == "0") {
        anyhow::bail!("disabled by ORCH_COW_WORKTREES=0");
    }
    let templates = crate::home::state_dir()?.join("templates");
    tokio::fs::create_dir_all(&templates).await?;
    let wt_parent = wt_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("worktree has no parent"))?;
    if !volume_can_clone(&templates, wt_parent).await {
        anyhow::bail!("volume cannot clone files");
    }

    let template = templates.join(project);
    let lock = template_lock(&template);
    let _guard = lock.lock().await;
    refresh_template(main_dir, &template, default_branch).await?;
    if template.join(".gitmodules").exists() {
        // A cloned submodule `.git` file would point into the template's gitdir
        anyhow::bail!("submodules are not supported");
    }

    let res = populate(main_dir, &template, wt_dir, branch).await;
    if res.is_err() {
        let _ = git(
            main_dir,
            &["worktree", "remove", "--force", &wt_dir.to_string_lossy()],
        )
        .await;
        let _ = tokio::fs::remove_dir_all(wt_dir).await;
        let _ = git(main_dir, &["worktree", "prune"]).await;
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let st = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    }

    // CI jobs on a clone-capable volume set this so a failed probe fails the test
    fn expect_clone() -> bool {
        std::env::var("ORCH_COW_EXPECT_CLONE").is_ok_and(|v| v == "1")
    }

    fn repo_with_origin() -> (tempfile::TempDir, tempfile::TempDir, tempfile::TempDir) {
        let remote = tempfile::tempdir().unwrap();
        sh(remote.path(), &["init", "--bare", "-b", "main"]);
        let project = tempfile::tempdir().unwrap();
        sh(
            project.path(),
            &["clone", remote.path().to_str().unwrap(), "."],
        );
        sh(project.path(), &["config", "user.email", "t@t.com"]);
        sh(project.path(), &["config", "user.name", "T"]);
        std::fs::write(project.path().join("a.txt"), "a\n").unwrap();
        std::fs::write(project.path().join("d.txt"), "d\n").unwrap();
        sh(project.path(), &["add", "."]);
        sh(project.path(), &["commit", "-m", "init"]);
        sh(project.path(), &["push", "origin", "HEAD:main"]);
        sh(project.path(), &["fetch", "origin"]);
        let home = tempfile::tempdir().unwrap();
        (remote, project, home)
    }

    #[tokio::test]
    #[serial_test::serial(orch_home)]
    async fn clone_gives_clean_worktree_or_falls_back() {
        let (_remote, project, home) = repo_with_origin();
        std::env::set_var("ORCH_HOME", home.path());
        let templates = home.path().join("state/templates");
        std::fs::create_dir_all(&templates).unwrap();
        let wt_dir = home.path().join("worktrees/p/task-1");
        std::fs::create_dir_all(wt_dir.parent().unwrap()).unwrap();
        sh(project.path(), &["branch", "task-1", "origin/main"]);

        // Volumes without clone support (ext4, HFS+) take the else branch
        let supported = volume_can_clone(&templates, wt_dir.parent().unwrap()).await;
        if expect_clone() {
            assert!(
                supported,
                "ORCH_COW_EXPECT_CLONE is set but the volume cannot clone"
            );
        }
        let res = create_worktree(project.path(), "p", &wt_dir, "task-1", "main").await;
        if supported {
            res.expect("clone path should succeed");
            assert_eq!(
                std::fs::read_to_string(wt_dir.join("a.txt")).unwrap(),
                "a\n"
            );
            let status = std::process::Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&wt_dir)
                .output()
                .unwrap();
            assert!(status.stdout.is_empty(), "clone must have a clean index");
            assert!(super::super::worktree::validate_worktree_gitdir(&wt_dir).await);
        } else {
            assert!(res.is_err());
            assert!(!wt_dir.exists(), "failed clone must leave no directory");
        }
        std::env::remove_var("ORCH_HOME");
    }

    #[tokio::test]
    #[serial_test::serial(orch_home)]
    async fn branch_differing_from_template_gets_its_own_files() {
        let (_remote, project, home) = repo_with_origin();
        std::env::set_var("ORCH_HOME", home.path());
        let templates = home.path().join("state/templates");
        std::fs::create_dir_all(&templates).unwrap();
        let wt_dir = home.path().join("worktrees/p/task-3");
        std::fs::create_dir_all(wt_dir.parent().unwrap()).unwrap();

        // Task branch: a.txt changed, b.txt added, d.txt deleted
        sh(
            project.path(),
            &["checkout", "-q", "-b", "task-3", "origin/main"],
        );
        std::fs::write(project.path().join("a.txt"), "changed\n").unwrap();
        std::fs::write(project.path().join("b.txt"), "b\n").unwrap();
        std::fs::remove_file(project.path().join("d.txt")).unwrap();
        sh(project.path(), &["add", "-A"]);
        sh(project.path(), &["commit", "-m", "task"]);
        sh(project.path(), &["checkout", "-q", "--detach"]);

        let supported = volume_can_clone(&templates, wt_dir.parent().unwrap()).await;
        if expect_clone() {
            assert!(
                supported,
                "ORCH_COW_EXPECT_CLONE is set but the volume cannot clone"
            );
        }
        if supported {
            create_worktree(project.path(), "p", &wt_dir, "task-3", "main")
                .await
                .expect("clone path should succeed");
            assert_eq!(
                std::fs::read_to_string(wt_dir.join("a.txt")).unwrap(),
                "changed\n"
            );
            assert_eq!(
                std::fs::read_to_string(wt_dir.join("b.txt")).unwrap(),
                "b\n"
            );
            assert!(!wt_dir.join("d.txt").exists());
            let status = std::process::Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&wt_dir)
                .output()
                .unwrap();
            assert!(status.stdout.is_empty(), "clone must have a clean index");
        }
        std::env::remove_var("ORCH_HOME");
    }

    #[tokio::test]
    #[serial_test::serial(orch_home)]
    async fn env_switch_disables_clone_path() {
        let (_remote, project, home) = repo_with_origin();
        std::env::set_var("ORCH_HOME", home.path());
        std::env::set_var("ORCH_COW_WORKTREES", "0");
        let wt_dir = home.path().join("worktrees/p/task-2");
        let res = create_worktree(project.path(), "p", &wt_dir, "task-2", "main").await;
        std::env::remove_var("ORCH_COW_WORKTREES");
        std::env::remove_var("ORCH_HOME");
        assert!(res.is_err());
        assert!(!wt_dir.exists());
    }
}
