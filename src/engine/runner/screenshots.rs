//! Screenshots on the PR. The agent lists image paths in its final JSON, orch pushes the images
//! to a branch that never merges and comments on the PR with links pinned to that commit

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process;

use anyhow::{Context, bail};
use tokio::process::Command;

use super::git_ops;
use crate::cmd::CommandErrorContext;
use crate::parser::Screenshot;

/// Branch that holds the images, used as no PR base or head, and cleanup deletes task branches only
pub const SCREENSHOTS_BRANCH: &str = "orch/screenshots";

const MAX_IMAGES: usize = 12;
const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];
const LOCAL_REF: &str = "refs/orch/screenshots";

/// An image checked and placed at its path on the screenshots branch
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub source: PathBuf,
    pub branch_path: String,
    pub caption: String,
}

/// Keep the file name safe for a URL and a git tree
fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    cleaned.trim_start_matches('.').to_string()
}

/// Check each listed image, and skip a bad entry with a reason so it does not drop the others
pub fn prepare(
    shots: &[Screenshot],
    base: &Path,
    roots: &[PathBuf],
    pr_number: u64,
) -> (Vec<Prepared>, Vec<String>) {
    let mut prepared = Vec::new();
    let mut skipped = Vec::new();

    for shot in shots {
        if prepared.len() >= MAX_IMAGES {
            skipped.push(format!("{}: more than {MAX_IMAGES} screenshots", shot.path));
            continue;
        }
        match check_image(&shot.path, base, roots) {
            Ok(source) => {
                let name = source
                    .file_name()
                    .map(|n| safe_name(&n.to_string_lossy()))
                    .unwrap_or_default();
                prepared.push(Prepared {
                    branch_path: format!("pr-{pr_number}/{}-{name}", prepared.len() + 1),
                    source,
                    caption: shot.caption.clone(),
                });
            }
            Err(e) => skipped.push(format!("{}: {e}", shot.path)),
        }
    }
    (prepared, skipped)
}

fn check_image(path: &str, base: &Path, roots: &[PathBuf]) -> anyhow::Result<PathBuf> {
    let joined = base.join(path.trim());
    let source = fs::canonicalize(&joined).context("file not found")?;
    if !roots.iter().any(|root| source.starts_with(root)) {
        bail!("outside the worktree");
    }
    let is_image = source
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
    if !is_image {
        bail!("not an image file");
    }
    let meta = fs::metadata(&source).context("unreadable")?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_IMAGE_BYTES {
        bail!("empty or larger than {} MB", MAX_IMAGE_BYTES / 1024 / 1024);
    }
    let mut head = [0u8; 12];
    let read = File::open(&source)
        .and_then(|mut f| f.read(&mut head))
        .context("unreadable")?;
    if !has_image_header(&head[..read]) {
        bail!("not an image file");
    }
    Ok(source)
}

fn has_image_header(head: &[u8]) -> bool {
    head.starts_with(b"\x89PNG\r\n\x1a\n")
        || head.starts_with(&[0xFF, 0xD8, 0xFF])
        || head.starts_with(b"GIF8")
        || (head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP"))
}

fn one_line(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(['[', ']'], "")
}

/// The PR comment, with links pinned to `commit` so they render in a private repository
pub fn build_comment(
    repo: &str,
    commit: &str,
    images: &[Prepared],
    note: Option<&str>,
    skipped: &[String],
) -> String {
    let mut body = String::from("## Screenshots\n");
    for (i, image) in images.iter().enumerate() {
        let caption = one_line(&image.caption);
        let alt = if caption.is_empty() {
            format!("Screenshot {}", i + 1)
        } else {
            caption.clone()
        };
        let url = format!(
            "https://github.com/{repo}/blob/{commit}/{}?raw=true",
            image.branch_path
        );
        body.push('\n');
        if !caption.is_empty() {
            let _ = writeln!(body, "**{caption}**\n");
        }
        let _ = writeln!(body, "![{alt}]({url})");
    }
    if !skipped.is_empty() {
        body.push_str("\nSkipped:\n");
        for reason in skipped {
            let _ = writeln!(body, "- {}", one_line(reason));
        }
    }
    if let Some(note) = note.map(str::trim).filter(|n| !n.is_empty()) {
        let _ = write!(body, "\n> {}\n", one_line(note));
    }
    body
}

async fn git_out(dir: &Path, index: Option<&Path>, args: &[&str]) -> anyhow::Result<String> {
    let mut cmd: Command = git_ops::git().await;
    cmd.arg("-C").arg(dir).args(args);
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    cmd.env("GIT_AUTHOR_NAME", "orch")
        .env("GIT_AUTHOR_EMAIL", "orch@localhost")
        .env("GIT_COMMITTER_NAME", "orch")
        .env("GIT_COMMITTER_EMAIL", "orch@localhost");
    let out = cmd.output_with_context().await?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Roots an image may live in: the worktree and its git dir, which stays out of the PR commit
pub async fn allowed_roots(worktree: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(root) = fs::canonicalize(worktree) {
        roots.push(root);
    }
    if let Ok(dir) = git_out(worktree, None, &["rev-parse", "--absolute-git-dir"]).await
        && let Ok(dir) = fs::canonicalize(dir)
    {
        roots.push(dir);
    }
    roots
}

/// Push the images on top of the screenshots branch, with a throwaway index so the worktree stays
/// untouched, and return the commit the links point at
pub async fn publish(
    worktree: &Path,
    images: &[Prepared],
    pr_number: u64,
) -> anyhow::Result<String> {
    if images.is_empty() {
        bail!("no images to publish");
    }
    let git_dir = git_out(worktree, None, &["rev-parse", "--absolute-git-dir"]).await?;
    let index = PathBuf::from(git_dir).join(format!("orch-screenshots-index-{}", process::id()));
    let result = push_images(worktree, &index, images, pr_number).await;
    let _ = fs::remove_file(&index);
    result
}

async fn push_images(
    worktree: &Path,
    index: &Path,
    images: &[Prepared],
    pr_number: u64,
) -> anyhow::Result<String> {
    let fetch_spec = format!("+refs/heads/{SCREENSHOTS_BRANCH}:{LOCAL_REF}");
    let parent = match git_out(worktree, None, &["fetch", "origin", &fetch_spec]).await {
        Ok(_) => git_out(worktree, None, &["rev-parse", "--verify", LOCAL_REF])
            .await
            .ok(),
        Err(_) => None,
    };

    if let Some(parent) = &parent {
        git_out(worktree, Some(index), &["read-tree", parent]).await?;
    }
    for image in images {
        let source = image.source.to_string_lossy();
        let blob = git_out(worktree, None, &["hash-object", "-w", "--", &source]).await?;
        let entry = format!("100644,{blob},{}", image.branch_path);
        git_out(
            worktree,
            Some(index),
            &["update-index", "--add", "--cacheinfo", &entry],
        )
        .await?;
    }
    let tree = git_out(worktree, Some(index), &["write-tree"]).await?;
    let message = format!("Screenshots for PR #{pr_number}");
    let mut args = vec!["commit-tree", &tree, "-m", &message];
    if let Some(parent) = &parent {
        args.extend(["-p", parent]);
    }
    let commit = git_out(worktree, None, &args).await?;

    let refspec = format!("{commit}:refs/heads/{SCREENSHOTS_BRANCH}");
    git_out(worktree, None, &["push", "origin", &refspec]).await?;
    Ok(commit)
}

#[cfg(test)]
mod tests {
    use std::process::Command as StdCommand;
    use std::slice;

    use super::*;

    const PNG_HEADER: &[u8] = b"\x89PNG\r\n\x1a\n";

    fn shot(path: &str, caption: &str) -> Screenshot {
        Screenshot {
            path: path.into(),
            caption: caption.into(),
        }
    }

    fn git_run(dir: &Path, args: &[&str]) -> String {
        let out = StdCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn comment_links_images_to_the_commit() {
        let images = vec![
            Prepared {
                source: PathBuf::from("/x/home.png"),
                branch_path: "pr-7/1-home.png".into(),
                caption: "Home, desktop".into(),
            },
            Prepared {
                source: PathBuf::from("/x/phone.png"),
                branch_path: "pr-7/2-phone.png".into(),
                caption: String::new(),
            },
        ];
        let body = build_comment(
            "owner/repo",
            "abc123",
            &images,
            Some("ui-screenshot skill missing"),
            &[],
        );
        assert!(body.starts_with("## Screenshots"));
        assert!(body.contains("**Home, desktop**"));
        assert!(body.contains(
            "![Home, desktop](https://github.com/owner/repo/blob/abc123/pr-7/1-home.png?raw=true)"
        ));
        assert!(body.contains(
            "![Screenshot 2](https://github.com/owner/repo/blob/abc123/pr-7/2-phone.png?raw=true)"
        ));
        assert!(body.contains("> ui-screenshot skill missing"));
    }

    #[test]
    fn note_comment_has_no_images() {
        let body = build_comment(
            "owner/repo",
            "",
            &[],
            Some("no browser, the screenshot could not be taken"),
            &[],
        );
        assert!(!body.contains("!["));
        assert!(body.contains("> no browser"));
    }

    #[test]
    fn prepare_keeps_images_inside_the_roots_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        fs::write(root.join("a b.png"), PNG_HEADER).unwrap();
        fs::write(root.join("notes.txt"), b"text").unwrap();
        fs::write(root.join("fake.png"), b"not an image at all").unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.png"), PNG_HEADER).unwrap();

        let shots = vec![
            shot("a b.png", "A"),
            shot("notes.txt", "B"),
            shot("fake.png", "F"),
            shot("missing.png", "C"),
            shot(&outside.path().join("secret.png").to_string_lossy(), "D"),
            shot("../secret.png", "E"),
        ];
        let (prepared, skipped) = prepare(&shots, &root, slice::from_ref(&root), 9);

        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0].branch_path, "pr-9/1-a-b.png");
        assert_eq!(skipped.len(), 5);
    }

    #[tokio::test]
    async fn publish_pushes_a_branch_the_worktree_never_sees() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let clone = tmp.path().join("clone");
        git_run(tmp.path(), &["init", "--bare", "-q", "origin.git"]);
        git_run(tmp.path(), &["init", "-q", "-b", "main", "clone"]);
        git_run(&clone, &["config", "user.email", "t@t"]);
        git_run(&clone, &["config", "user.name", "t"]);
        git_run(
            &clone,
            &["remote", "add", "origin", &origin.to_string_lossy()],
        );
        fs::write(clone.join("f"), "x").unwrap();
        git_run(&clone, &["add", "f"]);
        git_run(&clone, &["commit", "-qm", "init"]);
        let head_before = git_run(&clone, &["rev-parse", "HEAD"]);

        let shot_path = clone.join("home.png");
        fs::write(&shot_path, PNG_HEADER).unwrap();
        let roots = vec![fs::canonicalize(&clone).unwrap()];
        let (images, _) = prepare(&[shot("home.png", "Home")], &clone, &roots, 3);
        let first = publish(&clone, &images, 3).await.unwrap();

        fs::write(&shot_path, b"\x89PNG\r\n\x1a\nsecond").unwrap();
        let (images, _) = prepare(&[shot("home.png", "Home")], &clone, &roots, 4);
        let second = publish(&clone, &images, 4).await.unwrap();

        assert_ne!(first, second);
        let tip = format!("refs/heads/{SCREENSHOTS_BRANCH}");
        assert_eq!(git_run(&origin, &["rev-parse", &tip]), second);
        assert_eq!(
            git_run(&origin, &["rev-parse", &format!("{second}^")]),
            first
        );
        let files = git_run(&origin, &["ls-tree", "-r", "--name-only", &second]);
        assert!(files.contains("pr-3/1-home.png"), "{files}");
        assert!(files.contains("pr-4/1-home.png"), "{files}");
        assert_eq!(git_run(&clone, &["rev-parse", "HEAD"]), head_before);
        assert_eq!(
            git_run(&clone, &["branch", "--list", SCREENSHOTS_BRANCH]),
            ""
        );
    }
}
