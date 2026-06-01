use crate::cmd::SyncCommandErrorContext;
use crate::engine::runner::diff::parse_unified_diff_hunks;
use anyhow::Context;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::Path;

#[derive(Debug, Clone)]
struct ChangedFile {
    path: String,
    old_path: Option<String>,
}

#[derive(Debug, Clone)]
struct CommitPlan {
    message: String,
    files: Vec<ChangedFile>,
}

pub fn run(dry_run: bool, yes: bool, message_override: Option<String>) -> anyhow::Result<()> {
    ensure_git_repo()?;
    ensure_no_merge_conflicts()?;

    if !has_changes()? {
        println!("nothing to commit");
        return Ok(());
    }

    git_add_all()?;
    let files = collect_changed_files()?;
    if files.is_empty() {
        println!("nothing to commit");
        return Ok(());
    }

    let diff_text = git_output([
        "diff",
        "--cached",
        "--no-color",
        "--unified=0",
        "--no-ext-diff",
        "--find-renames",
    ])?;

    let plans = if let Some(message) = message_override {
        vec![CommitPlan { message, files }]
    } else {
        build_plans(files, &diff_text)
    };

    print_plan_summary(&plans);

    if dry_run {
        git_reset_unstage_all()?;
        return Ok(());
    }

    if !yes {
        match prompt_confirmation()? {
            UserChoice::No => {
                git_reset_unstage_all()?;
                println!("aborted");
                return Ok(());
            }
            UserChoice::Edit => {
                let mut edited = plans.clone();
                edit_messages(&mut edited)?;
                execute_plans(&edited)?;
                return Ok(());
            }
            UserChoice::Yes => {}
        }
    }

    execute_plans(&plans)
}

fn execute_plans(plans: &[CommitPlan]) -> anyhow::Result<()> {
    git_reset_unstage_all()?;

    for plan in plans {
        let mut path_set: BTreeSet<String> = BTreeSet::new();
        for file in &plan.files {
            if let Some(old) = &file.old_path {
                path_set.insert(old.clone());
            }
            path_set.insert(file.path.clone());
        }

        let mut cmd = std::process::Command::new("git");
        cmd.arg("add").arg("-A").arg("--");
        for path in path_set {
            cmd.arg(path);
        }
        let add = cmd.output_with_context()?;
        if !add.status.success() {
            anyhow::bail!(
                "failed to stage commit group: {}",
                String::from_utf8_lossy(&add.stderr).trim()
            );
        }

        let commit = std::process::Command::new("git")
            .args(["commit", "-m", &plan.message])
            .output_with_context()
            .context("failed to run git commit")?;
        if !commit.status.success() {
            anyhow::bail!(
                "git commit failed: {}",
                String::from_utf8_lossy(&commit.stderr).trim()
            );
        }
    }

    Ok(())
}

enum UserChoice {
    Yes,
    No,
    Edit,
}

fn prompt_confirmation() -> anyhow::Result<UserChoice> {
    print!("\nCommit these changes? [y/n/e] ");
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let value = input.trim().to_ascii_lowercase();
    Ok(match value.as_str() {
        "y" | "yes" => UserChoice::Yes,
        "e" | "edit" => UserChoice::Edit,
        _ => UserChoice::No,
    })
}

fn edit_messages(plans: &mut [CommitPlan]) -> anyhow::Result<()> {
    println!("\nEdit commit messages (press enter to keep current message):");
    for (i, plan) in plans.iter_mut().enumerate() {
        println!("\n{}. {}", i + 1, plan.message);
        print!("new message> ");
        io::stdout().flush().ok();
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let next = input.trim();
        if !next.is_empty() {
            plan.message = next.to_string();
        }
    }
    Ok(())
}

fn print_plan_summary(plans: &[CommitPlan]) {
    let file_count: usize = plans.iter().map(|p| p.files.len()).sum();
    println!(
        "\nProposed {} commits from {} changed files:\n",
        plans.len(),
        file_count
    );
    for (i, plan) in plans.iter().enumerate() {
        println!(
            "{}. {}",
            i + 1,
            plan.message.lines().next().unwrap_or("commit")
        );
        for file in &plan.files {
            if let Some(old) = &file.old_path {
                println!("   - {} -> {}", old, file.path);
            } else {
                println!("   - {}", file.path);
            }
        }
        if plan.message.lines().count() > 1 {
            for body_line in plan.message.lines().skip(1) {
                println!("   {}", body_line);
            }
        }
        println!();
    }
}

fn build_plans(files: Vec<ChangedFile>, diff_text: &str) -> Vec<CommitPlan> {
    let hunks = parse_unified_diff_hunks(diff_text);
    let mut hunk_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for file in &hunks {
        hunk_counts.insert(file.path.as_str(), file.hunks.len());
    }

    let mut groups: BTreeMap<String, Vec<ChangedFile>> = BTreeMap::new();

    for file in files {
        let key = group_key(&file.path);
        groups.entry(key).or_default().push(file);
    }

    let mut plans = Vec::new();
    for (_, mut grouped_files) in groups {
        grouped_files.sort_by(|a, b| a.path.cmp(&b.path));
        let message = message_for_group(&grouped_files, &hunk_counts);
        plans.push(CommitPlan {
            message,
            files: grouped_files,
        });
    }

    plans.sort_by(|a, b| a.message.cmp(&b.message));
    plans
}

fn group_key(path: &str) -> String {
    if is_doc(path) {
        return "docs".to_string();
    }
    if is_config(path) {
        return "config".to_string();
    }

    if is_test(path) {
        if let Some(module) = test_module(path) {
            return format!("test:{module}");
        }
        return "test".to_string();
    }

    let dir = Path::new(path)
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or("")
        .to_string();

    if dir.is_empty() {
        "root".to_string()
    } else {
        let parts: Vec<&str> = dir.split('/').collect();
        if parts.first().copied() == Some("src") && parts.len() > 1 {
            format!("src:{}", parts[1])
        } else {
            format!("dir:{dir}")
        }
    }
}

fn message_for_group(files: &[ChangedFile], hunk_counts: &BTreeMap<&str, usize>) -> String {
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();

    let kind = if paths.iter().all(|p| is_doc(p)) {
        "docs"
    } else if paths.iter().all(|p| is_test(p)) {
        "test"
    } else if paths.iter().all(|p| is_config(p)) {
        "chore"
    } else if files.iter().any(|f| f.old_path.is_some()) {
        "refactor"
    } else {
        "feat"
    };

    let scope = infer_scope(&paths);
    let subject = if files.len() == 1 {
        format!("update {}", paths[0])
    } else {
        format!("update {} files", scope.as_deref().unwrap_or("project"))
    };

    let header = if let Some(scope) = &scope {
        format!("{kind}({scope}): {subject}")
    } else {
        format!("{kind}: {subject}")
    };

    let total_hunks: usize = files
        .iter()
        .map(|f| hunk_counts.get(f.path.as_str()).copied().unwrap_or(0))
        .sum();

    if total_hunks > 3 || files.len() > 2 {
        let mut body = String::new();
        body.push_str("\n\nIncludes:\n");
        for file in files {
            body.push_str("- ");
            body.push_str(&file.path);
            body.push('\n');
        }
        format!("{header}{body}")
    } else {
        header
    }
}

fn infer_scope(paths: &[&str]) -> Option<String> {
    let first = paths.first()?;
    let parent = Path::new(first)
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(|s| s.to_string());

    if paths.iter().all(|p| {
        Path::new(p)
            .parent()
            .and_then(|pp| pp.file_name())
            .and_then(|n| n.to_str())
            .map(|s| Some(s.to_string()) == parent)
            .unwrap_or(false)
    }) {
        parent
    } else {
        None
    }
}

fn is_doc(path: &str) -> bool {
    path.ends_with(".md") || path.starts_with("docs/")
}

fn is_test(path: &str) -> bool {
    path.starts_with("tests/") || path.contains("_test.") || path.ends_with(".spec.ts")
}

fn test_module(path: &str) -> Option<String> {
    let stem = Path::new(path).file_stem()?.to_str()?;
    Some(stem.trim_end_matches("_test").to_string())
}

fn is_config(path: &str) -> bool {
    matches!(
        path,
        "Cargo.toml"
            | "Cargo.lock"
            | "package.json"
            | "bun.lockb"
            | "bun.lock"
            | "pnpm-lock.yaml"
            | "package-lock.json"
    ) || path.starts_with(".github/workflows/")
}

fn ensure_git_repo() -> anyhow::Result<()> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .output_with_context()?;
    if !out.status.success() {
        anyhow::bail!("not a git repository");
    }
    Ok(())
}

fn ensure_no_merge_conflicts() -> anyhow::Result<()> {
    let out = git_output(["diff", "--name-only", "--diff-filter=U"])?;
    if !out.trim().is_empty() {
        anyhow::bail!("merge conflicts detected; resolve conflicts before committing");
    }
    Ok(())
}

fn has_changes() -> anyhow::Result<bool> {
    let unstaged = std::process::Command::new("git")
        .args(["diff", "--quiet"])
        .status_with_context()?;
    let staged = std::process::Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .status_with_context()?;
    let untracked = git_output(["ls-files", "--others", "--exclude-standard"])?;

    Ok(!unstaged.success() || !staged.success() || !untracked.trim().is_empty())
}

fn git_add_all() -> anyhow::Result<()> {
    let out = std::process::Command::new("git")
        .args(["add", "-A"])
        .output_with_context()?;
    if !out.status.success() {
        anyhow::bail!(
            "git add -A failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

fn git_reset_unstage_all() -> anyhow::Result<()> {
    let out = std::process::Command::new("git")
        .args(["reset"])
        .output_with_context()?;
    if !out.status.success() {
        anyhow::bail!("git reset failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

fn collect_changed_files() -> anyhow::Result<Vec<ChangedFile>> {
    let name_status = git_output(["diff", "--cached", "--name-status", "--find-renames"])?;
    let mut files = Vec::new();

    for line in name_status.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.is_empty() {
            continue;
        }
        let status = parts[0];
        if status.starts_with('R') || status.starts_with('C') {
            if parts.len() >= 3 {
                files.push(ChangedFile {
                    path: parts[2].to_string(),
                    old_path: Some(parts[1].to_string()),
                });
            }
        } else if parts.len() >= 2 {
            files.push(ChangedFile {
                path: parts[1].to_string(),
                old_path: None,
            });
        }
    }

    Ok(files)
}

fn git_output<const N: usize>(args: [&str; N]) -> anyhow::Result<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .output_with_context()?;
    if !out.status.success() {
        anyhow::bail!("git {} failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::{build_plans, collect_changed_files, git_output, run};
    use serial_test::serial;
    use std::process::Command;

    fn init_temp_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .arg("init")
            .current_dir(dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("README.md"), "hello\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        dir
    }

    #[test]
    fn groups_docs_and_code_separately() {
        let files = vec![
            super::ChangedFile {
                path: "README.md".to_string(),
                old_path: None,
            },
            super::ChangedFile {
                path: "src/main.rs".to_string(),
                old_path: None,
            },
        ];

        let plans = build_plans(files, "");
        assert_eq!(plans.len(), 2);
    }

    #[test]
    #[serial]
    fn dry_run_works_in_temp_repo() {
        let dir = init_temp_repo();
        std::env::set_current_dir(dir.path()).unwrap();
        std::fs::write(dir.path().join("src.rs"), "fn main() {}\n").unwrap();

        run(true, true, None).unwrap();

        let log = git_output(["log", "--oneline", "-1"]).unwrap();
        assert!(log.contains("init"));
    }

    #[test]
    #[serial]
    fn commits_in_temp_repo_with_yes() {
        let dir = init_temp_repo();
        std::env::set_current_dir(dir.path()).unwrap();
        std::fs::write(dir.path().join("notes.md"), "updated\n").unwrap();

        run(false, true, None).unwrap();

        let count = Command::new("git")
            .args(["rev-list", "--count", "HEAD"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let count = String::from_utf8_lossy(&count.stdout)
            .trim()
            .parse::<u32>()
            .unwrap();
        assert!(count >= 2);
    }

    #[test]
    #[serial]
    fn parses_name_status() {
        let dir = init_temp_repo();
        std::env::set_current_dir(dir.path()).unwrap();
        std::fs::write(dir.path().join("x.txt"), "a\n").unwrap();
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let files = collect_changed_files().unwrap();
        assert!(files.iter().any(|f| f.path == "x.txt"));
    }
}
