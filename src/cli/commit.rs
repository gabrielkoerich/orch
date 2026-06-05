use crate::cmd::SyncCommandErrorContext;
use crate::engine::runner::diff::parse_unified_diff_hunks;
use anyhow::Context;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::Path;

/// Default cap on diff bytes sent to the LLM per commit group.
const DEFAULT_MAX_DIFF_BYTES: usize = 32 * 1024;

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

pub async fn run(
    dry_run: bool,
    yes: bool,
    message_override: Option<String>,
    no_llm: bool,
) -> anyhow::Result<()> {
    ensure_git_repo()?;
    ensure_no_merge_conflicts()?;

    if !has_changes()? {
        println!("nothing to commit");
        return Ok(());
    }

    // Collect files and diff WITHOUT touching the index, preserving the user's staging state.
    let files = collect_all_changed_files()?;
    if files.is_empty() {
        println!("nothing to commit");
        return Ok(());
    }

    let diff_text = get_full_diff()?;

    let mut plans = if let Some(message) = message_override {
        vec![CommitPlan { message, files }]
    } else {
        let mut p = build_plans(files, &diff_text);
        if !no_llm {
            enrich_with_llm(&mut p).await;
        }
        p
    };

    print_plan_summary(&plans);

    if dry_run {
        // Index was never mutated — nothing to undo.
        return Ok(());
    }

    if !yes {
        match prompt_confirmation()? {
            UserChoice::No => {
                // Index was never mutated — nothing to undo.
                println!("aborted");
                return Ok(());
            }
            UserChoice::Edit => {
                edit_messages(&mut plans)?;
                execute_plans(&plans)?;
                return Ok(());
            }
            UserChoice::Yes => {}
        }
    }

    execute_plans(&plans)
}

/// Replace each plan's heuristic message with an LLM-drafted one.
///
/// On any error (no store, no agent, cooldowns, timeout) the heuristic
/// message survives and a one-line warning is logged.
async fn enrich_with_llm(plans: &mut [CommitPlan]) {
    let store = match crate::cli::init_store().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "orch commit: could not open store for LLM message; using heuristic messages"
            );
            return;
        }
    };

    let agent = match crate::control::get_agent(&store).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "orch commit: could not resolve agent; using heuristic messages");
            return;
        }
    };
    let model = match crate::control::get_model(&store).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "orch commit: could not resolve model; using heuristic messages");
            return;
        }
    };

    let max_bytes = resolve_max_diff_bytes();

    for plan in plans.iter_mut() {
        match draft_message_for_plan(&agent, &model, plan, max_bytes).await {
            Ok(message) => plan.message = message,
            Err(e) => tracing::warn!(
                agent = %agent,
                model = %model,
                error = %e,
                "orch commit: LLM draft failed, keeping heuristic message"
            ),
        }
    }
}

fn resolve_max_diff_bytes() -> usize {
    crate::config::get("commit.max_diff_bytes")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_DIFF_BYTES)
}

/// Call the agent once for this plan and return a cleaned commit message.
async fn draft_message_for_plan(
    agent: &str,
    model: &str,
    plan: &CommitPlan,
    max_bytes: usize,
) -> anyhow::Result<String> {
    let paths: Vec<String> = plan
        .files
        .iter()
        .flat_map(|f| std::iter::once(f.path.clone()).chain(f.old_path.clone()))
        .collect();

    let diff_text = diff_for_paths(&paths)?;
    let (diff_for_prompt, truncated) = truncate_diff(&diff_text, max_bytes);

    let file_list = plan
        .files
        .iter()
        .map(|f| match &f.old_path {
            Some(old) => format!("- {} -> {}", old, f.path),
            None => format!("- {}", f.path),
        })
        .collect::<Vec<_>>()
        .join("\n");

    let truncated_note = if truncated {
        format!(
            "Note: the diff was truncated to ~{} KB to fit the budget. Base the message on the visible portion.",
            max_bytes / 1024
        )
    } else {
        String::new()
    };

    let prompt = include_str!("../../prompts/commit_message.md")
        .replace("{{FILES}}", &file_list)
        .replace("{{DIFF}}", &diff_for_prompt)
        .replace("{{TRUNCATED_NOTE}}", &truncated_note);

    let result = crate::control::invoke_agent(agent, model, "", &prompt).await?;
    let cleaned = clean_llm_message(&result.text);
    if cleaned.is_empty() {
        anyhow::bail!("LLM returned an empty commit message");
    }
    Ok(cleaned)
}

/// Strip fences, leading/trailing whitespace, and obvious chatter from an LLM response.
fn clean_llm_message(raw: &str) -> String {
    let trimmed = raw.trim();

    // Remove a leading triple-backtick fence if present (with or without a language tag).
    let without_open_fence = if let Some(rest) = trimmed.strip_prefix("```") {
        // Drop the rest of the opening line (language tag) up to the first newline.
        match rest.find('\n') {
            Some(i) => &rest[i + 1..],
            None => rest,
        }
    } else {
        trimmed
    };

    // Remove a trailing triple-backtick fence.
    let without_close_fence = without_open_fence
        .trim_end()
        .strip_suffix("```")
        .unwrap_or(without_open_fence);

    without_close_fence.trim().to_string()
}

/// Truncate the diff at `max_bytes`, snapping to a UTF-8 character boundary.
fn truncate_diff(diff: &str, max_bytes: usize) -> (String, bool) {
    if diff.len() <= max_bytes {
        return (diff.to_string(), false);
    }
    let mut cutoff = max_bytes;
    while cutoff > 0 && !diff.is_char_boundary(cutoff) {
        cutoff -= 1;
    }
    (diff[..cutoff].to_string(), true)
}

/// Get the combined staged + unstaged + untracked diff scoped to the given paths.
///
/// `git diff --cached` and `git diff` never include untracked files, so for any
/// path returned by `git ls-files --others --exclude-standard` we synthesize a
/// `new file` diff via `git diff --no-index /dev/null <path>`. Without this the
/// LLM would see only the filename for brand-new files and fall back to
/// path-only guessing — defeating the point of the LLM draft.
///
/// Uses default unified context (3 lines) so the LLM can see what changed
/// in surrounding code, unlike `get_full_diff()` which uses --unified=0
/// because it only feeds the hunk-counting heuristic.
fn diff_for_paths(paths: &[String]) -> anyhow::Result<String> {
    let unique: BTreeSet<String> = paths.iter().cloned().collect();

    let untracked_list = git_output(["ls-files", "--others", "--exclude-standard"])?;
    let untracked: BTreeSet<String> = untracked_list
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();

    let mut staged_args: Vec<String> = vec![
        "diff".into(),
        "--cached".into(),
        "--no-color".into(),
        "--no-ext-diff".into(),
        "--find-renames".into(),
        "--".into(),
    ];
    staged_args.extend(unique.iter().cloned());

    let mut unstaged_args: Vec<String> = vec![
        "diff".into(),
        "--no-color".into(),
        "--no-ext-diff".into(),
        "--find-renames".into(),
        "--".into(),
    ];
    unstaged_args.extend(unique.iter().cloned());

    let staged = run_git(&staged_args)?;
    let unstaged = run_git(&unstaged_args)?;

    let mut combined = format!("{staged}{unstaged}");
    for path in unique.iter().filter(|p| untracked.contains(*p)) {
        combined.push_str(&diff_for_untracked(path)?);
    }
    Ok(combined)
}

/// Produce a unified diff for an untracked file by diffing it against /dev/null.
///
/// `git diff --no-index` exits 1 when the files differ — that is the expected
/// path here, since we are always comparing to an empty file. Treat exit 0 and
/// 1 as success and surface anything else as an error.
fn diff_for_untracked(path: &str) -> anyhow::Result<String> {
    let out = std::process::Command::new("git")
        .args([
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-index",
            "--",
            "/dev/null",
            path,
        ])
        .output_with_context()?;
    let code = out.status.code();
    if !matches!(code, Some(0) | Some(1)) {
        anyhow::bail!(
            "git diff --no-index -- /dev/null {path} failed (exit {:?}): {}",
            code,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn run_git(args: &[String]) -> anyhow::Result<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .output_with_context()?;
    if !out.status.success() {
        anyhow::bail!("git {} failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
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

fn git_reset_unstage_all() -> anyhow::Result<()> {
    let out = std::process::Command::new("git")
        .args(["reset"])
        .output_with_context()?;
    if !out.status.success() {
        anyhow::bail!("git reset failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

/// Collect all changed files (staged + unstaged + untracked) without touching the index.
fn collect_all_changed_files() -> anyhow::Result<Vec<ChangedFile>> {
    let mut seen = BTreeSet::new();
    let mut files = Vec::new();

    let staged = git_output(["diff", "--cached", "--name-status", "--find-renames"])?;
    for line in staged.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(cf) = parse_name_status_line(line) {
            if seen.insert(cf.path.clone()) {
                files.push(cf);
            }
        }
    }

    let unstaged = git_output(["diff", "--name-status", "--find-renames"])?;
    for line in unstaged.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(cf) = parse_name_status_line(line) {
            if seen.insert(cf.path.clone()) {
                files.push(cf);
            }
        }
    }

    let untracked = git_output(["ls-files", "--others", "--exclude-standard"])?;
    for line in untracked.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if seen.insert(line.to_string()) {
            files.push(ChangedFile {
                path: line.to_string(),
                old_path: None,
            });
        }
    }

    Ok(files)
}

fn parse_name_status_line(line: &str) -> Option<ChangedFile> {
    let parts: Vec<&str> = line.split('\t').collect();
    let status = parts.first()?;
    if status.starts_with('R') || status.starts_with('C') {
        if parts.len() >= 3 {
            Some(ChangedFile {
                path: parts[2].to_string(),
                old_path: Some(parts[1].to_string()),
            })
        } else {
            None
        }
    } else if parts.len() >= 2 {
        Some(ChangedFile {
            path: parts[1].to_string(),
            old_path: None,
        })
    } else {
        None
    }
}

/// Returns combined staged + unstaged diff for analysis without mutating the index.
fn get_full_diff() -> anyhow::Result<String> {
    let staged = git_output([
        "diff",
        "--cached",
        "--no-color",
        "--unified=0",
        "--no-ext-diff",
        "--find-renames",
    ])?;
    let unstaged = git_output([
        "diff",
        "--no-color",
        "--unified=0",
        "--no-ext-diff",
        "--find-renames",
    ])?;
    Ok(format!("{staged}{unstaged}"))
}

#[cfg(test)]
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

    #[tokio::test]
    #[serial]
    async fn dry_run_works_in_temp_repo() {
        let dir = init_temp_repo();
        std::env::set_current_dir(dir.path()).unwrap();
        std::fs::write(dir.path().join("src.rs"), "fn main() {}\n").unwrap();

        run(true, true, None, true).await.unwrap();

        let log = git_output(["log", "--oneline", "-1"]).unwrap();
        assert!(log.contains("init"));
    }

    #[tokio::test]
    #[serial]
    async fn commits_in_temp_repo_with_yes() {
        let dir = init_temp_repo();
        std::env::set_current_dir(dir.path()).unwrap();
        std::fs::write(dir.path().join("notes.md"), "updated\n").unwrap();

        run(false, true, None, true).await.unwrap();

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

    #[tokio::test]
    #[serial]
    async fn message_override_skips_llm_and_commits_once() {
        let dir = init_temp_repo();
        std::env::set_current_dir(dir.path()).unwrap();
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        std::fs::write(dir.path().join("b.md"), "b\n").unwrap();

        // no_llm=false on purpose — the override must take priority over the LLM path.
        run(
            false,
            true,
            Some("chore: forced override".to_string()),
            false,
        )
        .await
        .unwrap();

        let log = git_output(["log", "--oneline", "-1"]).unwrap();
        assert!(
            log.contains("chore: forced override"),
            "expected override message in HEAD, got: {log}"
        );
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

    #[test]
    fn clean_llm_message_strips_fences_and_whitespace() {
        let raw = "```\nfeat(cli): add --no-llm flag\n\nLet users opt out of LLM-drafted messages.\n```\n";
        let cleaned = super::clean_llm_message(raw);
        assert_eq!(
            cleaned,
            "feat(cli): add --no-llm flag\n\nLet users opt out of LLM-drafted messages."
        );
    }

    #[test]
    fn clean_llm_message_strips_language_tagged_fence() {
        let raw = "```text\nfix(router): handle empty pool\n```";
        let cleaned = super::clean_llm_message(raw);
        assert_eq!(cleaned, "fix(router): handle empty pool");
    }

    #[test]
    fn clean_llm_message_passes_plain_text_through() {
        let cleaned = super::clean_llm_message("  docs: clarify usage  \n\n");
        assert_eq!(cleaned, "docs: clarify usage");
    }

    #[test]
    fn truncate_diff_no_op_when_under_budget() {
        let diff = "diff --git a/x b/x\n+hello\n";
        let (out, truncated) = super::truncate_diff(diff, 1024);
        assert!(!truncated);
        assert_eq!(out, diff);
    }

    #[test]
    fn truncate_diff_snaps_to_char_boundary() {
        let diff = "héllo wörld"; // multi-byte chars in the middle
        let (out, truncated) = super::truncate_diff(diff, 5);
        assert!(truncated);
        // out must be valid UTF-8 and never split a multi-byte char.
        assert!(out.chars().count() > 0);
        assert!(diff.starts_with(&out));
    }

    #[test]
    #[serial]
    fn diff_for_paths_includes_untracked_file_contents() {
        let dir = init_temp_repo();
        std::env::set_current_dir(dir.path()).unwrap();
        // Brand-new file: never staged, never tracked. The LLM path must still
        // see its contents — otherwise the message would be drafted from the
        // filename alone, which is exactly the regression this guards against.
        std::fs::write(dir.path().join("brand_new.txt"), "alpha\nbeta\ngamma\n").unwrap();

        let diff = super::diff_for_paths(&["brand_new.txt".to_string()]).unwrap();
        assert!(
            diff.contains("brand_new.txt"),
            "diff should reference the new file path, got: {diff}"
        );
        assert!(
            diff.contains("+alpha") && diff.contains("+beta") && diff.contains("+gamma"),
            "diff should include the new file's added lines, got: {diff}"
        );
    }
}
