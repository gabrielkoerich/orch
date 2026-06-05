//! Home directory utilities.
//!
//! All orch state lives under `~/.orch/`. This is completely separate from
//! the legacy bash orchestrator's `~/.orchestrator/` directory — both tools can
//! run side by side without conflicts.

use anyhow::Context;
use std::path::{Path, PathBuf};

/// The home directory name.
const HOME_DIR: &str = ".orch";

fn ensure_dir(path: &Path) -> anyhow::Result<()> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| std::fs::create_dir_all(path))?;
        }
        _ => std::fs::create_dir_all(path)?,
    }

    Ok(())
}

/// Get the orch home directory path (~/.orch/).
///
/// Can be overridden via the `ORCH_HOME` environment variable. This is useful
/// in tests and CI environments to avoid reading/writing the real state directory.
pub fn orch_home() -> anyhow::Result<PathBuf> {
    if let Ok(dir) = std::env::var("ORCH_HOME") {
        let path = PathBuf::from(dir);
        ensure_dir(&path)?;
        return Ok(path);
    }
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    let path = home.join(HOME_DIR);
    ensure_dir(&path)?;
    Ok(path)
}

/// Get the orch state directory path (~/.orch/state/).
///
/// This is where runtime state like logs, prompts, and PID files are stored.
/// Used across the codebase for runtime state file resolution.
pub fn state_dir() -> anyhow::Result<PathBuf> {
    let home = orch_home()?;
    let state = home.join("state");
    ensure_dir(&state)?;
    Ok(state)
}

/// Legacy state directories for backward-compatible reads.
///
/// Checks legacy `~/.orchestrator/state/` first, then `~/.orchestrator/.orchestrator/`.
/// Never writes to these locations.
fn legacy_state_dir() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    // Check legacy ~/.orchestrator/state/ (intermediate migration path)
    let mid = home.join(".orchestrator").join("state");
    if mid.is_dir() {
        return Some(mid);
    }
    // Check legacy ~/.orchestrator/.orchestrator/ (original nested path)
    let old = home.join(".orchestrator").join(".orchestrator");
    if old.is_dir() {
        return Some(old);
    }
    None
}

/// Resolve a file inside the state directory, falling back to the legacy
/// location when the file doesn't exist at the new path yet.
pub fn state_file(name: &str) -> anyhow::Result<PathBuf> {
    let new_path = state_dir()?.join(name);
    if new_path.exists() {
        return Ok(new_path);
    }
    // Check legacy location
    if let Some(legacy) = legacy_state_dir() {
        let old_path = legacy.join(name);
        if old_path.exists() {
            return Ok(old_path);
        }
    }
    // Return new path even if it doesn't exist yet (for writes)
    Ok(new_path)
}

/// Get the path to the global config file (~/.orch/config.yml).
pub fn config_path() -> anyhow::Result<PathBuf> {
    Ok(orch_home()?.join("config.yml"))
}

/// Get the path to the tasks database file (~/.orch/orch.db).
///
/// Also migrates legacy `orchestrator.db` → `orch.db` transparently if the old file
/// exists and the new one does not.
pub async fn db_path() -> anyhow::Result<PathBuf> {
    let home = orch_home()?;
    let new_path = home.join("orch.db");
    let old_path = home.join("orchestrator.db");
    if old_path.exists() && !new_path.exists() {
        tokio::fs::rename(&old_path, &new_path)
            .await
            .with_context(|| {
                format!(
                    "migrating database from {} to {}",
                    old_path.display(),
                    new_path.display()
                )
            })?;
        tracing::info!("migrated database: legacy orchestrator.db -> orch.db");
    }
    Ok(new_path)
}

/// Get the path to the service version file (~/.orch/state/service.version).
///
/// Written by the engine at startup, deleted on graceful shutdown.
/// Format: "{pid}\t{version}" — the PID lets readers verify the owning process is alive.
/// Used by service status and diagnostics to detect CLI/service drift.
pub fn service_version_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(state_dir()?.join("service.version"))
}

/// Returns `true` if the given PID is alive and is an `orch serve` process.
pub fn pid_is_orch_serve(pid: u32) -> bool {
    process_args(pid)
        .as_deref()
        .is_some_and(command_line_is_orch_serve)
}

/// Find all PIDs of running `orch serve` processes.
pub fn find_orch_serve_pids(exclude_self: bool) -> Vec<u32> {
    let output = std::process::Command::new("pgrep")
        .args(["-x", "orch"])
        .output()
        .ok();

    let self_pid = std::process::id();
    match output {
        Some(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .filter(|pid| !exclude_self || *pid != self_pid)
            .filter(|pid| pid_is_orch_serve(*pid))
            .collect(),
        _ => Vec::new(),
    }
}

/// Send SIGTERM to other running `orch serve` processes, then SIGKILL stragglers.
pub fn kill_other_orch_serve_processes(timeout: std::time::Duration) -> u32 {
    let others = find_orch_serve_pids(true);
    if others.is_empty() {
        return 0;
    }

    for pid in &others {
        tracing::info!(pid, "sending SIGTERM to stale orch serve process");
        send_signal(*pid, "TERM");
    }

    let mut remaining = wait_for_orch_serve_exit(others.clone(), timeout);
    for pid in &remaining {
        tracing::warn!(pid, "stale orch serve ignored SIGTERM; sending SIGKILL");
        send_signal(*pid, "KILL");
    }

    remaining = wait_for_orch_serve_exit(remaining, std::time::Duration::from_secs(1));
    for pid in &remaining {
        tracing::error!(pid, "stale orch serve survived SIGKILL");
    }

    (others.len() - remaining.len()) as u32
}

fn process_args(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "args="])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|args| !args.is_empty())
}

fn command_line_is_orch_serve(cmdline: &str) -> bool {
    let mut args = cmdline.split_whitespace();
    let Some(program) = args.next() else {
        return false;
    };
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "orch")
        && args.any(|arg| arg == "serve")
}

fn send_signal(pid: u32, signal: &str) {
    let _ = std::process::Command::new("kill")
        .args([format!("-{signal}"), pid.to_string()])
        .output();
}

fn wait_for_orch_serve_exit(mut pids: Vec<u32>, timeout: std::time::Duration) -> Vec<u32> {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout && !pids.is_empty() {
        std::thread::sleep(std::time::Duration::from_millis(200));
        pids.retain(|pid| pid_is_orch_serve(*pid));
    }
    pids
}

/// Get the path to the worktrees directory (~/.orch/worktrees/).
pub fn worktrees_dir() -> anyhow::Result<PathBuf> {
    let dir = orch_home()?.join("worktrees");
    ensure_dir(&dir)?;
    Ok(dir)
}

/// Get the path to the contexts directory (~/.orch/contexts/).
pub fn contexts_dir() -> anyhow::Result<PathBuf> {
    let dir = orch_home()?.join("contexts");
    ensure_dir(&dir)?;
    Ok(dir)
}

/// Get the path to the projects directory (~/.orch/projects/).
pub fn projects_dir() -> anyhow::Result<PathBuf> {
    let dir = orch_home()?.join("projects");
    ensure_dir(&dir)?;
    Ok(dir)
}

/// Get the path to the skills directory (~/.orch/skills/).
pub fn skills_dir() -> anyhow::Result<PathBuf> {
    let dir = orch_home()?.join("skills");
    ensure_dir(&dir)?;
    Ok(dir)
}

/// Get the per-repo state directory: `~/.orch/state/{owner}/{repo}/`.
///
/// Falls back to the flat `~/.orch/state/` if no repo is configured.
pub fn repo_state_dir(repo: &str) -> anyhow::Result<PathBuf> {
    let state = state_dir()?;
    let dir = state.join(repo.replace('/', std::path::MAIN_SEPARATOR_STR));
    ensure_dir(&dir)?;
    Ok(dir)
}

/// Get the per-task directory: `~/.orch/state/{owner}/{repo}/tasks/{id}/`.
///
/// Creates the directory on demand.
pub fn task_dir(repo: &str, task_id: &str) -> anyhow::Result<PathBuf> {
    let dir = repo_state_dir(repo)?.join("tasks").join(task_id);
    ensure_dir(&dir)?;
    Ok(dir)
}

/// Async version of [`task_dir`] — uses `tokio::fs::create_dir_all` to avoid
/// blocking the Tokio executor when creating a new directory.
pub async fn task_dir_async(repo: &str, task_id: &str) -> anyhow::Result<PathBuf> {
    let state = state_dir()?;
    let dir = state
        .join(repo.replace('/', std::path::MAIN_SEPARATOR_STR))
        .join("tasks")
        .join(task_id);
    tokio::fs::create_dir_all(&dir).await?;
    Ok(dir)
}

/// Get the per-task attempt directory: `~/.orch/state/{owner}/{repo}/tasks/{id}/attempts/{n}/`.
///
/// Creates the directory on demand.
pub fn task_attempt_dir(repo: &str, task_id: &str, attempt: u32) -> anyhow::Result<PathBuf> {
    let dir = task_dir(repo, task_id)?
        .join("attempts")
        .join(attempt.to_string());
    ensure_dir(&dir)?;
    Ok(dir)
}

/// Async version of [`task_attempt_dir`] — uses `tokio::fs::create_dir_all` to
/// avoid blocking the Tokio executor when creating a new directory.
pub async fn task_attempt_dir_async(
    repo: &str,
    task_id: &str,
    attempt: u32,
) -> anyhow::Result<PathBuf> {
    let state = state_dir()?;
    let dir = state
        .join(repo.replace('/', std::path::MAIN_SEPARATOR_STR))
        .join("tasks")
        .join(task_id)
        .join("attempts")
        .join(attempt.to_string());
    tokio::fs::create_dir_all(&dir).await?;
    Ok(dir)
}

/// Pre-create all standard orch directories using async I/O.
///
/// Call once at engine startup before the Tokio thread pool is busy so that
/// subsequent synchronous callers of [`orch_home`], [`state_dir`], etc. find
/// the directories already present and their `create_dir_all` calls become
/// fast no-ops (a single `stat` syscall on an existing path).
pub async fn init_dirs() -> anyhow::Result<()> {
    let home = orch_home()?;
    tokio::fs::create_dir_all(&home).await?;
    tokio::fs::create_dir_all(home.join("state")).await?;
    tokio::fs::create_dir_all(home.join("worktrees")).await?;
    tokio::fs::create_dir_all(home.join("contexts")).await?;
    tokio::fs::create_dir_all(home.join("projects")).await?;
    tokio::fs::create_dir_all(home.join("skills")).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Helper to set ORCH_HOME to a temp dir for the duration of a test.
    ///
    /// Returns the temp dir (keep alive for the test) and the previous value of
    /// ORCH_HOME so the caller can restore it in the test teardown.
    fn with_temp_orch_home() -> (TempDir, Option<String>) {
        let temp = TempDir::new().unwrap();
        let orch_home = temp.path().join(".orch");
        std::fs::create_dir_all(&orch_home).unwrap();
        let prev = std::env::var("ORCH_HOME").ok();
        std::env::set_var("ORCH_HOME", &orch_home);
        (temp, prev)
    }

    fn restore_orch_home(prev: Option<String>) {
        match prev {
            Some(v) => std::env::set_var("ORCH_HOME", v),
            None => std::env::remove_var("ORCH_HOME"),
        }
    }

    #[test]
    fn test_orch_home_env_override() {
        let temp = TempDir::new().unwrap();
        let custom_dir = temp.path().join(".orch-custom");
        let prev = std::env::var("ORCH_HOME").ok();
        std::env::set_var("ORCH_HOME", &custom_dir);

        let result = orch_home().unwrap();
        assert_eq!(result, custom_dir);
        assert!(result.exists());

        restore_orch_home(prev);
    }

    #[test]
    fn test_task_dir_creates_path() {
        // Use ORCH_HOME so we don't touch the developer's real home dir.
        let (_temp, prev) = with_temp_orch_home();

        let dir = task_dir("test-owner/test-repo", "42").unwrap();
        assert!(dir.exists());
        assert!(dir.ends_with("test-owner/test-repo/tasks/42"));

        restore_orch_home(prev);
    }

    #[test]
    fn test_task_attempt_dir_creates_path() {
        // Use ORCH_HOME so we don't touch the developer's real home dir.
        let (_temp, prev) = with_temp_orch_home();

        let dir = task_attempt_dir("test-owner/test-repo", "42", 1).unwrap();
        assert!(dir.exists());
        assert!(dir.ends_with("test-owner/test-repo/tasks/42/attempts/1"));

        restore_orch_home(prev);
    }

    #[test]
    fn test_repo_state_dir_separates_repos() {
        let (_temp, prev) = with_temp_orch_home();

        let dir_a = repo_state_dir("owner/repo-a").unwrap();
        let dir_b = repo_state_dir("owner/repo-b").unwrap();
        assert_ne!(dir_a, dir_b);

        restore_orch_home(prev);
    }

    #[test]
    fn pid_is_orch_serve_returns_false_for_nonexistent_pid() {
        assert!(!pid_is_orch_serve(99999));
    }

    #[test]
    fn command_line_is_orch_serve_matches_real_invocations() {
        assert!(command_line_is_orch_serve("/opt/homebrew/bin/orch serve"));
        assert!(command_line_is_orch_serve(
            "/opt/homebrew/Cellar/orch/0.73.18/bin/orch serve --foreground"
        ));
        assert!(!command_line_is_orch_serve("/opt/homebrew/bin/orch -V"));
        assert!(!command_line_is_orch_serve("/bin/sh -c orch serve"));
        assert!(!command_line_is_orch_serve("/tmp/not-orch serve"));
    }

    #[test]
    fn find_orch_serve_pids_does_not_panic() {
        let pids = find_orch_serve_pids(false);
        assert!(pids.iter().all(|pid| pid_is_orch_serve(*pid)));
    }

    #[test]
    fn find_orch_serve_pids_excludes_self() {
        let self_pid = std::process::id();
        let pids = find_orch_serve_pids(true);
        assert!(!pids.contains(&self_pid));
    }
}
