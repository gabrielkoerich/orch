use crate::sys::cmd::CommandErrorContext;
use std::collections::BTreeMap;
use std::path::Path;
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiffSummary {
    pub path: String,
    pub hunks: Vec<DiffHunk>,
}

pub fn parse_unified_diff_hunks(diff: &str) -> Vec<FileDiffSummary> {
    let mut by_file: BTreeMap<String, Vec<DiffHunk>> = BTreeMap::new();
    let mut current_file: Option<String> = None;

    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_file = Some(path.to_string());
            by_file.entry(path.to_string()).or_default();
            continue;
        }

        if let Some(rest) = line.strip_prefix("@@ -") {
            let Some(file) = current_file.as_ref() else {
                continue;
            };
            if let Some(hunk) = parse_hunk_header(rest) {
                by_file.entry(file.clone()).or_default().push(hunk);
            }
        }
    }

    by_file
        .into_iter()
        .map(|(path, hunks)| FileDiffSummary { path, hunks })
        .collect()
}

fn parse_hunk_header(rest: &str) -> Option<DiffHunk> {
    let (old_part, remainder) = rest.split_once(" +")?;
    let (new_part, _) = remainder.split_once(" @@")?;

    let (old_start, old_count) = parse_range(old_part)?;
    let (new_start, new_count) = parse_range(new_part)?;

    Some(DiffHunk {
        old_start,
        old_count,
        new_start,
        new_count,
    })
}

fn parse_range(part: &str) -> Option<(u32, u32)> {
    if let Some((start, count)) = part.split_once(',') {
        Some((start.parse().ok()?, count.parse().ok()?))
    } else {
        Some((part.parse().ok()?, 1))
    }
}

/// Check if the current branch has any commits ahead of the default branch.
pub async fn has_commits_ahead(dir: &Path, default_branch: &str) -> bool {
    let output = Command::new("git")
        .args([
            "rev-list",
            "--count",
            &format!("origin/{default_branch}..HEAD"),
        ])
        .current_dir(dir)
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => {
            let count: u32 = String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse()
                .unwrap_or(0);
            count > 0
        }
        _ => {
            tracing::debug!(
                default_branch,
                "git rev-list failed for compare ref, assuming commits ahead"
            );
            // If we can't determine, assume there ARE commits ahead so we
            // attempt a push (better to try and fail loudly than silently
            // skip push and lose the PR creation path).
            true
        }
    }
}

/// Count the number of changed files in the working directory.
pub async fn count_changed_files(dir: &Path) -> anyhow::Result<usize> {
    // Count modified and new files (excluding deleted)
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .output_with_context()
        .await?;

    let status_output = String::from_utf8_lossy(&output.stdout);
    let count = status_output
        .lines()
        .filter(|line| {
            let prefix = line.get(0..2).unwrap_or("");
            // Count modified (M), added (A), renamed (R), copied (C), untracked (??)
            prefix.starts_with('M')
                || prefix.starts_with('A')
                || prefix.starts_with('R')
                || prefix.starts_with('C')
                || prefix.starts_with('?')
        })
        .count();

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::parse_unified_diff_hunks;

    #[test]
    fn parses_multi_file_hunks() {
        let diff = r"diff --git a/src/a.rs b/src/a.rs
index 1111111..2222222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,1 +1,2 @@
+line
@@ -10,2 +11,3 @@
-line
+line
+line2
diff --git a/src/b.rs b/src/b.rs
index 3333333..4444444 100644
--- a/src/b.rs
+++ b/src/b.rs
@@ -5 +5 @@
-old
+new
";

        let files = parse_unified_diff_hunks(diff);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[0].hunks.len(), 2);
        assert_eq!(files[1].path, "src/b.rs");
        assert_eq!(files[1].hunks.len(), 1);
    }
}
