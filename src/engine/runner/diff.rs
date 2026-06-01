use std::collections::BTreeMap;

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

#[cfg(test)]
mod tests {
    use super::parse_unified_diff_hunks;

    #[test]
    fn parses_multi_file_hunks() {
        let diff = r#"diff --git a/src/a.rs b/src/a.rs
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
"#;

        let files = parse_unified_diff_hunks(diff);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[0].hunks.len(), 2);
        assert_eq!(files[1].path, "src/b.rs");
        assert_eq!(files[1].hunks.len(), 1);
    }
}
