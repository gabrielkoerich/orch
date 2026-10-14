//! `orch stream` and `orch task watch`: tail each running agent's `output.json` through the
//! NDJSON formatter. The runner tees agent stdout there, so lines arrive whole

use crate::cli::ndjson::{self, Formatter};
use std::collections::HashMap;
use std::fs::File;
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

// Replayed when a file is first opened, so a new viewer sees recent context
const BACKLOG_BYTES: u64 = 64 * 1024;

struct Tail {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
    skip_first: bool,
    formatter: Formatter,
}

impl Tail {
    fn open(path: PathBuf, color: bool) -> Self {
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let offset = len.saturating_sub(BACKLOG_BYTES);
        Self {
            path,
            offset,
            partial: Vec::new(),
            skip_first: offset > 0,
            formatter: Formatter::new(color),
        }
    }

    /// Complete lines appended since the last call
    fn read_lines(&mut self) -> Vec<String> {
        let mut buf = Vec::new();
        let read = File::open(&self.path).and_then(|mut f| {
            f.seek(SeekFrom::Start(self.offset))?;
            f.read_to_end(&mut buf)
        });
        let Ok(n) = read else {
            return vec![];
        };

        self.offset += n as u64;
        self.partial.extend_from_slice(&buf);
        let Some(end) = self.partial.iter().rposition(|&b| b == b'\n') else {
            return vec![];
        };

        let rest = self.partial.split_off(end + 1);
        let complete = std::mem::replace(&mut self.partial, rest);
        let mut lines: Vec<String> = String::from_utf8_lossy(&complete)
            .lines()
            .map(String::from)
            .collect();
        // The backlog starts mid-line
        if std::mem::take(&mut self.skip_first) && !lines.is_empty() {
            lines.remove(0);
        }

        lines
    }
}

/// Follow one task (and its review) when `task_id` is set, else every running agent session
pub async fn run(task_id: Option<&str>, formatted: bool) -> anyhow::Result<()> {
    let color = formatted && std::io::stdout().is_terminal();
    match task_id {
        Some(id) => println!("Watching task {id}. Ctrl+C to stop"),
        None => println!("Watching all running agents. Ctrl+C to stop"),
    }

    let mut tails: HashMap<String, Tail> = HashMap::new();
    let mut colors: HashMap<String, &str> = HashMap::new();
    let mut tag_width = 0;
    let mut said_waiting = false;
    loop {
        let found = sources(task_id).await?;
        if let (Some(id), true, false) = (task_id, found.is_empty(), said_waiting) {
            println!("No output for task {id} yet, waiting for it to start");
            said_waiting = true;
        }

        let live: Vec<String> = found.iter().map(|(key, _)| key.clone()).collect();
        for (key, path) in found {
            let tail = tails
                .entry(key)
                .or_insert_with(|| Tail::open(path.clone(), color));
            if tail.path != path {
                *tail = Tail::open(path, color);
            }
        }

        // A session that just ended gets this last read, its final message lands after the work
        for tail in tails.values_mut() {
            let mut entries = Vec::new();
            for line in tail.read_lines() {
                if formatted {
                    entries.extend(tail.formatter.format(&line));
                } else {
                    entries.push(line);
                }
            }
            if entries.is_empty() {
                continue;
            }

            // Output scrolls fast, so every entry carries its task and attempt
            let tag = tag(&tail.path);
            tag_width = tag_width.max(tag.chars().count());
            let next = TAG_COLORS[colors.len() % TAG_COLORS.len()];
            let code = *colors.entry(key_of(&tail.path)).or_insert(next);
            let gutter = |show: bool| {
                let text = if show { tag.as_str() } else { "" };
                let text = format!("{text:<tag_width$} │");
                if color {
                    format!("\x1b[{code}m{text}\x1b[0m")
                } else {
                    text
                }
            };
            for line in tag_lines(&entries, formatted, gutter) {
                println!("{line}");
            }
        }
        tails.retain(|key, _| live.contains(key));

        std::io::stdout().flush()?;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Output files to follow, keyed by the session or task they belong to
async fn sources(task_id: Option<&str>) -> anyhow::Result<Vec<(String, PathBuf)>> {
    if let Some(id) = task_id {
        let names = watch_dir_names(id, task_dir_exists)?;
        return Ok(latest_output(None, &names)
            .map(|p| (id.to_string(), p))
            .into_iter()
            .collect());
    }

    let tmux = crate::tmux::TmuxManager::new();
    Ok(crate::channels::tmux::list_orch_sessions()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|session| {
            let (project, task) = tmux.parse_session_name(&session)?;
            let path = latest_output(Some(&project), &[task_dir_name(&task)])?;
            Some((session, path))
        })
        .collect())
}

/// Task dirs for a watched id and its review. A bare number may name an internal task, unless
/// a GitHub task has the same number
fn watch_dir_names(task_id: &str, exists: impl Fn(&str) -> bool) -> anyhow::Result<Vec<String>> {
    let name = task_dir_name(task_id);
    let base = name.strip_suffix("-review").unwrap_or(&name);
    let with_review = |b: &str| vec![format!("{b}-review"), b.to_string()];
    if base.starts_with("internal:") {
        return Ok(with_review(base));
    }

    let github = with_review(base);
    let internal = with_review(&format!("internal:{base}"));
    let has_github = github.iter().any(|n| exists(n));
    let has_internal = internal.iter().any(|n| exists(n));
    match (has_github, has_internal) {
        (true, true) => anyhow::bail!(
            "{base} matches GitHub task {base} and internal task internal:{base}, \
             watch internal:{base} for the internal one"
        ),
        (false, true) => Ok(internal),
        _ => Ok(github),
    }
}

/// Whether any repo has state for a task dir named `name`
fn task_dir_exists(name: &str) -> bool {
    let Ok(state) = crate::home::state_dir() else {
        return false;
    };
    subdirs(&state)
        .iter()
        .flat_map(|owner| subdirs(owner))
        .any(|repo| repo.join("tasks").join(name).is_dir())
}

/// Session names replace `:` with `-`, task dirs keep the `internal:` id
fn task_dir_name(task_id: &str) -> String {
    match task_id.strip_prefix("internal-") {
        Some(rest) => format!("internal:{rest}"),
        None => task_id.to_string(),
    }
}

/// Newest `output.json` under `state/<owner>/<repo>/tasks/<name>/attempts/*`, in any repo unless `project` is set
fn latest_output(project: Option<&str>, names: &[String]) -> Option<PathBuf> {
    let state = crate::home::state_dir().ok()?;
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for owner in subdirs(&state) {
        for repo in subdirs(&owner) {
            if project.is_some_and(|p| repo.file_name() != Some(p.as_ref())) {
                continue;
            }

            for name in names {
                for attempt in subdirs(&repo.join("tasks").join(name).join("attempts")) {
                    let output = attempt.join("output.json");
                    let Ok(modified) = output.metadata().and_then(|m| m.modified()) else {
                        continue;
                    };
                    if best.as_ref().is_none_or(|(t, _)| modified > *t) {
                        best = Some((modified, output));
                    }
                }
            }
        }
    }

    best.map(|(_, p)| p)
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

const TAG_COLORS: [&str; 6] = ["36", "35", "33", "34", "96", "95"];

/// Entry lines with `gutter(true)` on each step's first line and `gutter(false)` on the rest
fn tag_lines(entries: &[String], formatted: bool, gutter: impl Fn(bool) -> String) -> Vec<String> {
    let mut out = Vec::new();
    for entry in entries {
        let step = !formatted || !entry.starts_with(ndjson::RESULT_PREFIX);
        if formatted && step {
            out.push(gutter(false));
        }

        for (i, line) in entry.lines().enumerate() {
            out.push(format!("{} {line}", gutter(step && i == 0)));
        }
    }
    out
}

/// `<repo>#<task> a<n>` from `.../<repo>/tasks/<task>/attempts/<n>/output.json`
fn tag(path: &Path) -> String {
    match path_parts(path).as_slice() {
        [attempt, _, task, _, repo] => {
            let task = task.strip_prefix("internal:").unwrap_or(task);
            format!("{repo}#{task} a{attempt}")
        }
        _ => path.display().to_string(),
    }
}

/// Task dir of an output file, so a task keeps its colour across attempts
fn key_of(path: &Path) -> String {
    path.ancestors()
        .nth(3)
        .map_or_else(String::new, |p| p.display().to_string())
}

fn path_parts(path: &Path) -> Vec<String> {
    path.iter()
        .rev()
        .skip(1)
        .take(5)
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_returns_only_complete_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output.json");
        std::fs::write(&path, "one\ntw").unwrap();
        let mut tail = Tail::open(path.clone(), false);
        assert_eq!(tail.read_lines(), vec!["one"]);

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"o\nthree\n").unwrap();
        assert_eq!(tail.read_lines(), vec!["two", "three"]);
        assert!(tail.read_lines().is_empty());
    }

    #[test]
    fn tail_skips_cut_line_in_backlog() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output.json");
        let long = "x".repeat(BACKLOG_BYTES as usize);
        std::fs::write(&path, format!("{long}\nlast\n")).unwrap();
        assert_eq!(Tail::open(path, false).read_lines(), vec!["last"]);
    }

    #[test]
    fn task_dir_name_restores_internal_colon() {
        assert_eq!(task_dir_name("internal-8-review"), "internal:8-review");
        assert_eq!(task_dir_name("3601"), "3601");
    }

    #[test]
    fn watch_dir_names_resolve_bare_and_prefixed_ids() {
        let only = |dirs: &'static [&'static str]| move |n: &str| dirs.contains(&n);

        // Bare id of an internal task
        assert_eq!(
            watch_dir_names("189497", only(&["internal:189497"])).unwrap(),
            vec!["internal:189497-review", "internal:189497"]
        );
        // Bare id of a GitHub task, or of a task with no state yet
        assert_eq!(
            watch_dir_names("3601-review", only(&["3601"])).unwrap(),
            vec!["3601-review", "3601"]
        );
        assert_eq!(
            watch_dir_names("42", only(&[])).unwrap(),
            vec!["42-review", "42"]
        );
        // Prefixed ids never look at GitHub tasks
        assert_eq!(
            watch_dir_names("internal:8", only(&["8"])).unwrap(),
            vec!["internal:8-review", "internal:8"]
        );
    }

    #[test]
    fn watch_dir_names_reject_ambiguous_bare_id() {
        let both = |n: &str| n == "77" || n == "internal:77-review";
        let err = watch_dir_names("77", both).unwrap_err().to_string();
        assert!(err.contains("watch internal:77"), "{err}");
    }

    #[test]
    fn tag_drops_internal_prefix() {
        let path = Path::new("/s/owner/bean/tasks/internal:189488-review/attempts/1/output.json");
        assert_eq!(tag(path), "bean#189488-review a1");
        assert_eq!(key_of(path), "/s/owner/bean/tasks/internal:189488-review");
    }

    #[test]
    fn tag_lines_label_each_step_only() {
        let entries = vec![
            "⏺ Read(a.rs)".to_string(),
            "  ⎿  Read 3 lines".to_string(),
            "⏺ Fixing it.\n  Then test.".to_string(),
        ];
        let gutter = |show: bool| if show { "t1 │" } else { "   │" }.to_string();
        assert_eq!(
            tag_lines(&entries, true, gutter),
            vec![
                "   │",
                "t1 │ ⏺ Read(a.rs)",
                "   │   ⎿  Read 3 lines",
                "   │",
                "t1 │ ⏺ Fixing it.",
                "   │   Then test.",
            ]
        );
    }

    #[test]
    fn tag_lines_raw_tags_every_line() {
        let entries = vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string()];
        let gutter = |show: bool| if show { "t │" } else { "  │" }.to_string();
        assert_eq!(
            tag_lines(&entries, false, gutter),
            vec!["t │ {\"a\":1}", "t │ {\"b\":2}"]
        );
    }
}
