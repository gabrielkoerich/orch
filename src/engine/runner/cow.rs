//! Copy-on-write task worktrees cloned from a per-project template by the git-cow crate, see docs/content/cow-worktrees.md

use super::worktree::resolve_branch_start_point;
use std::path::{Path, PathBuf};

fn template_dir(project: &str) -> anyhow::Result<PathBuf> {
    Ok(crate::home::state_dir()?.join("templates").join(project))
}

// Called after a task PR merges and before its worktree is removed.
// Projects with a working checkout are warmed from that checkout instead
pub async fn warm_from_merged(repo_root: &Path, project: &str, wt_dir: &Path) {
    let (repo, wt) = (repo_root.to_path_buf(), wt_dir.to_path_buf());
    let Ok(template) = template_dir(project) else {
        return;
    };
    let res = tokio::task::spawn_blocking(move || {
        let bare =
            git_cow::git(&repo, &["rev-parse", "--is-bare-repository"]).is_ok_and(|v| v == "true");
        let wt_parent = wt.parent().unwrap_or(&wt);
        let templates = template.parent().unwrap_or(&template);
        if !bare || !template.join(".git").exists() || !git_cow::can_clone(templates, wt_parent) {
            return Ok(());
        }
        git_cow::warm(&wt, &template)
    })
    .await;
    if let Ok(Err(e)) = res {
        tracing::debug!(error = %e, "template warm-up from merged task failed");
    }
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
    let template = template_dir(project)?;
    let start =
        resolve_branch_start_point(&main_dir.to_string_lossy(), default_branch, default_branch)
            .await;
    let (repo, wt, branch) = (
        main_dir.to_path_buf(),
        wt_dir.to_path_buf(),
        branch.to_string(),
    );
    tokio::task::spawn_blocking(move || {
        git_cow::add(&repo, &template, &start, &wt, None, Some(&branch))
    })
    .await??;
    Ok(())
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
        let supported = git_cow::can_clone(&templates, wt_dir.parent().unwrap());
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

        let supported = git_cow::can_clone(&templates, wt_dir.parent().unwrap());
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
    async fn checkout_ignored_dirs_reach_template_and_clone() {
        let (_remote, project, home) = repo_with_origin();
        std::fs::write(
            project.path().join(".gitignore"),
            "out/\n.env\n.local/\nnode/\n",
        )
        .unwrap();
        sh(project.path(), &["add", "."]);
        sh(project.path(), &["commit", "-m", "ignore"]);
        sh(project.path(), &["push", "origin", "HEAD:main"]);
        sh(project.path(), &["fetch", "origin"]);
        std::fs::create_dir_all(project.path().join("out/sub")).unwrap();
        std::fs::write(project.path().join("out/sub/dep.bin"), "dep\n").unwrap();
        std::fs::create_dir_all(project.path().join(".local")).unwrap();
        std::fs::write(project.path().join(".local/settings"), "x\n").unwrap();
        std::fs::create_dir_all(project.path().join("pkg/a/node")).unwrap();
        std::fs::write(project.path().join("pkg/a/node/dep.bin"), "nested\n").unwrap();
        std::fs::create_dir_all(project.path().join("pkg/a/.hidden/node")).unwrap();
        std::fs::write(project.path().join("pkg/a/.hidden/node/x"), "x\n").unwrap();
        std::fs::write(project.path().join("pkg/a/keep.txt"), "k\n").unwrap();
        std::fs::write(project.path().join(".env"), "secret\n").unwrap();

        std::env::set_var("ORCH_HOME", home.path());
        let templates = home.path().join("state/templates");
        std::fs::create_dir_all(&templates).unwrap();
        let wt_dir = home.path().join("worktrees/p/task-4");
        std::fs::create_dir_all(wt_dir.parent().unwrap()).unwrap();
        sh(project.path(), &["branch", "task-4", "origin/main"]);

        let supported = git_cow::can_clone(&templates, wt_dir.parent().unwrap());
        if expect_clone() {
            assert!(
                supported,
                "ORCH_COW_EXPECT_CLONE is set but the volume cannot clone"
            );
        }
        if supported {
            create_worktree(project.path(), "p", &wt_dir, "task-4", "main")
                .await
                .expect("clone path should succeed");
            let tpl = templates.join("p");
            for root in [&tpl, &wt_dir] {
                assert_eq!(
                    std::fs::read_to_string(root.join("out/sub/dep.bin")).unwrap(),
                    "dep\n"
                );
                assert!(!root.join(".env").exists(), "loose ignored file was copied");
                assert!(!root.join(".local").exists(), "hidden dir was copied");
                assert!(
                    !root.join("pkg/a/.hidden").exists(),
                    "nested hidden dir was copied"
                );
                assert_eq!(
                    std::fs::read_to_string(root.join("pkg/a/node/dep.bin")).unwrap(),
                    "nested\n"
                );
            }
            // The worktree keeps its own gitdir
            assert!(wt_dir.join(".git").is_file());
            assert_ne!(
                std::fs::read_to_string(wt_dir.join(".git")).unwrap(),
                std::fs::read_to_string(tpl.join(".git")).unwrap()
            );
            // A second refresh replaces the directory instead of merging into it
            std::fs::remove_file(project.path().join("out/sub/dep.bin")).unwrap();
            std::fs::write(project.path().join("out/new.bin"), "new\n").unwrap();
            let wt2 = home.path().join("worktrees/p/task-5");
            sh(project.path(), &["branch", "task-5", "origin/main"]);
            create_worktree(project.path(), "p", &wt2, "task-5", "main")
                .await
                .unwrap();
            assert!(wt2.join("out/new.bin").exists());
            assert!(!wt2.join("out/sub/dep.bin").exists());
        }
        std::env::remove_var("ORCH_HOME");
    }

    #[tokio::test]
    #[serial_test::serial(orch_home)]
    async fn merged_task_ignored_dirs_reach_template_of_bare_project() {
        let (remote, project, home) = repo_with_origin();
        std::fs::write(project.path().join(".gitignore"), "out/\n").unwrap();
        sh(project.path(), &["add", "."]);
        sh(project.path(), &["commit", "-m", "ignore"]);
        sh(project.path(), &["push", "origin", "HEAD:main"]);
        let bare = home.path().join("bare.git");
        sh(
            home.path(),
            &[
                "clone",
                "--bare",
                remote.path().to_str().unwrap(),
                "bare.git",
            ],
        );
        sh(&bare, &["branch", "task-6", "main"]);

        std::env::set_var("ORCH_HOME", home.path());
        let templates = home.path().join("state/templates");
        std::fs::create_dir_all(&templates).unwrap();
        let wt_dir = home.path().join("worktrees/pb/task-6");
        std::fs::create_dir_all(wt_dir.parent().unwrap()).unwrap();

        let supported = git_cow::can_clone(&templates, wt_dir.parent().unwrap());
        if expect_clone() {
            assert!(
                supported,
                "ORCH_COW_EXPECT_CLONE is set but the volume cannot clone"
            );
        }
        if supported {
            create_worktree(&bare, "pb", &wt_dir, "task-6", "main")
                .await
                .expect("clone path should succeed");
            let tpl = templates.join("pb");
            assert!(!tpl.join("out").exists());

            std::fs::create_dir_all(wt_dir.join("out")).unwrap();
            std::fs::write(wt_dir.join("out/dep.bin"), "dep\n").unwrap();
            std::fs::write(wt_dir.join("loose.log"), "x\n").unwrap();

            // A project with a working checkout is not warmed from tasks
            warm_from_merged(project.path(), "pb", &wt_dir).await;
            assert!(!tpl.join("out").exists());

            warm_from_merged(&bare, "pb", &wt_dir).await;
            assert_eq!(
                std::fs::read_to_string(tpl.join("out/dep.bin")).unwrap(),
                "dep\n"
            );
            assert!(!tpl.join("loose.log").exists());
            assert!(tpl.join(".git").is_file());
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
