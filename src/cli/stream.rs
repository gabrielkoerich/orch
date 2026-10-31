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
        let len = std::fs::metadata(&path).map_or(0, |m| m.len());
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
    let watched = if let Some(id) = task_id {
        println!("Watching task {id}. Ctrl+C to stop");
        Some(Watched::resolve(id).await?)
    } else {
        println!("Watching all running agents. Ctrl+C to stop");
        None
    };

    let mut tails: HashMap<String, Tail> = HashMap::new();
    let mut colors: HashMap<String, &str> = HashMap::new();
    let mut tag_width = 0;
    let mut said_waiting = false;
    loop {
        let found = match &watched {
            Some(task) => task.sources(),
            None => session_sources().await,
        };
        if let (Some(task), true, false) = (&watched, found.is_empty(), said_waiting) {
            println!("{}", task.nothing_yet());
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

/// The task `orch task watch <id>` follows, resolved once through the shared CLI helper
struct Watched {
    id: String,
    status: String,
    /// The task's review dir and its own
    dirs: Vec<PathBuf>,
}

impl Watched {
    async fn resolve(id: &str) -> anyhow::Result<Self> {
        let store = crate::cli::init_store().await?;
        // The review dir is watched anyway, so `<id>-review` watches the same pair
        let id = id.strip_suffix("-review").unwrap_or(id);
        let task = crate::cli::task_ref::resolve(&store, id).await?;
        Ok(Self {
            id: task.display_spec(),
            status: task.status.clone(),
            dirs: crate::cli::task_ref::state_dirs(&task)?,
        })
    }

    fn sources(&self) -> Vec<(String, PathBuf)> {
        newest_output(&self.dirs)
            .map(|path| (self.id.clone(), path))
            .into_iter()
            .collect()
    }

    /// A running task with no output file is a problem, so name the path instead of waiting quietly
    fn nothing_yet(&self) -> String {
        if self.status != crate::store::TaskStatus::InProgress.as_str() {
            return format!(
                "No output for task {} yet, waiting for it to start",
                self.id
            );
        }

        let searched: Vec<String> = self
            .dirs
            .iter()
            .map(|dir| dir.join("attempts").display().to_string())
            .collect();
        format!(
            "Task {} is in_progress but no output file was found under {}",
            self.id,
            searched.join(" or ")
        )
    }
}

/// Output files of every running agent session, keyed by session name
async fn session_sources() -> Vec<(String, PathBuf)> {
    let tmux = crate::tmux::TmuxManager::new();
    crate::channels::tmux::list_orch_sessions()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|session| {
            let (project, task) = tmux.parse_session_name(&session)?;
            let path = session_output(&project, &task_dir_name(&task))?;
            Some((session, path))
        })
        .collect()
}

/// Session names replace `:` with `-`, task dirs keep the `internal:` id
fn task_dir_name(task: &str) -> String {
    match task.strip_prefix("internal-") {
        Some(rest) => format!("internal:{rest}"),
        None => task.to_string(),
    }
}

/// A tmux session names the repo without its owner, so look under every owner
fn session_output(project: &str, name: &str) -> Option<PathBuf> {
    let state = crate::home::state_dir().ok()?;
    let dirs: Vec<PathBuf> = subdirs(&state)
        .iter()
        .flat_map(|owner| subdirs(owner))
        .filter(|repo| repo.file_name() == Some(project.as_ref()))
        .map(|repo| repo.join("tasks").join(name))
        .collect();
    newest_output(&dirs)
}

/// Newest `output.json` under `<dir>/attempts/*` for any of `dirs`
fn newest_output(dirs: &[PathBuf]) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for dir in dirs {
        for attempt in subdirs(&dir.join("attempts")) {
            let output = attempt.join("output.json");
            let Ok(modified) = output.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if best.as_ref().is_none_or(|(t, _)| modified > *t) {
                best = Some((modified, output));
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
        assert_eq!(tail.read_lines(), [] as [String; 0]);
    }

    #[test]
    fn tail_skips_cut_line_in_backlog() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output.json");
        let long = "x".repeat(usize::try_from(BACKLOG_BYTES).unwrap());

        std::fs::write(&path, format!("{long}\nlast\n")).unwrap();
        assert_eq!(Tail::open(path, false).read_lines(), vec!["last"]);
    }

    #[test]
    fn task_dir_name_restores_internal_colon() {
        assert_eq!(task_dir_name("internal-8-review"), "internal:8-review");
        assert_eq!(task_dir_name("3601"), "3601");
    }

    #[test]
    fn in_progress_without_output_names_the_path() {
        let watched = Watched {
            id: "owner/repo#5".to_string(),
            status: crate::store::TaskStatus::InProgress.as_str().to_string(),
            dirs: vec![
                PathBuf::from("/state/owner/repo/tasks/5-review"),
                PathBuf::from("/state/owner/repo/tasks/5"),
            ],
        };
        let msg = watched.nothing_yet();
        assert!(msg.contains("in_progress"), "{msg}");
        assert!(msg.contains("/state/owner/repo/tasks/5/attempts"), "{msg}");
        assert!(
            msg.contains("/state/owner/repo/tasks/5-review/attempts"),
            "{msg}"
        );

        let waiting = Watched {
            status: crate::store::TaskStatus::Routed.as_str().to_string(),
            ..watched
        };
        let msg = waiting.nothing_yet();
        assert!(msg.contains("waiting for it to start"), "{msg}");
        assert!(!msg.contains("no output file was found"), "{msg}");
    }

    #[test]
    fn newest_output_picks_the_latest_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let task = dir.path().join("tasks").join("5");
        for attempt in ["1", "2"] {
            let d = task.join("attempts").join(attempt);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("output.json"), attempt).unwrap();
            // mtime resolution is coarse on some filesystems
            std::thread::sleep(Duration::from_millis(10));
        }
        let found = newest_output(&[task.join("5-review"), task.clone()]).unwrap();
        assert_eq!(found, task.join("attempts").join("2").join("output.json"));
        assert!(newest_output(&[dir.path().join("missing")]).is_none());
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
