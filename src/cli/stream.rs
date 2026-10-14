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
    let mut last_label = String::new();
    loop {
        for (key, path) in sources(task_id).await {
            let tail = tails
                .entry(key)
                .or_insert_with(|| Tail::open(path.clone(), color));
            if tail.path != path {
                *tail = Tail::open(path, color);
            }

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

            let label = label(&tail.path);
            if label != last_label {
                let header = format!("── {label} ──");
                if color {
                    println!("\n\x1b[1;36m{header}\x1b[0m");
                } else {
                    println!("\n{header}");
                }
                last_label = label;
            }

            if formatted {
                ndjson::print(&entries);
            } else {
                entries.iter().for_each(|l| println!("{l}"));
            }
        }

        std::io::stdout().flush()?;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Output files to follow, keyed by the session or task they belong to
async fn sources(task_id: Option<&str>) -> Vec<(String, PathBuf)> {
    if let Some(id) = task_id {
        let name = task_dir_name(id);
        let name = name.strip_suffix("-review").unwrap_or(&name).to_string();
        let review = format!("{name}-review");
        return latest_output(None, &[name, review])
            .map(|p| (id.to_string(), p))
            .into_iter()
            .collect();
    }

    let tmux = crate::tmux::TmuxManager::new();
    crate::channels::tmux::list_orch_sessions()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|session| {
            let (project, task) = tmux.parse_session_name(&session)?;
            let path = latest_output(Some(&project), &[task_dir_name(&task)])?;
            Some((session, path))
        })
        .collect()
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

/// `<repo> <task> · attempt <n>` from `.../<repo>/tasks/<task>/attempts/<n>/output.json`
fn label(path: &Path) -> String {
    let parts: Vec<String> = path
        .iter()
        .rev()
        .skip(1)
        .take(5)
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    match parts.as_slice() {
        [attempt, _, task, _, repo] => format!("{repo} {task} · attempt {attempt}"),
        _ => path.display().to_string(),
    }
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
    fn label_from_attempt_path() {
        let path = Path::new("/s/owner/repo/tasks/3601-review/attempts/2/output.json");
        assert_eq!(label(path), "repo 3601-review · attempt 2");
    }
}
