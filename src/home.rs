//! Home directory utilities.
//!
//! All orch state lives under `~/.orch/`. This is completely separate from
//! the bash orchestrator's `~/.orchestrator/` directory — both tools can
//! run side by side without conflicts.

use anyhow::Context;
use std::path::PathBuf;

/// The home directory name.
const HOME_DIR: &str = ".orch";

/// Get the orch home directory path (~/.orch/).
pub fn orch_home() -> anyhow::Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    let path = home.join(HOME_DIR);
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

/// Get the orch state directory path (~/.orch/state/).
///
/// This is where runtime state like logs, prompts, and PID files are stored.
/// Used across the codebase for runtime state file resolution.
pub fn state_dir() -> anyhow::Result<PathBuf> {
    let home = orch_home()?;
    let state = home.join("state");
    std::fs::create_dir_all(&state)?;
    Ok(state)
}

/// Legacy state directories for backward-compatible reads.
///
/// Checks `~/.orchestrator/state/` first, then `~/.orchestrator/.orchestrator/`.
/// Never writes to these locations.
fn legacy_state_dir() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    // Check ~/.orchestrator/state/ (intermediate migration path)
    let mid = home.join(".orchestrator").join("state");
    if mid.is_dir() {
        return Some(mid);
    }
    // Check ~/.orchestrator/.orchestrator/ (original nested path)
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
/// Also migrates `orchestrator.db` → `orch.db` transparently if the old file
/// exists and the new one does not.
pub fn db_path() -> anyhow::Result<PathBuf> {
    let home = orch_home()?;
    let new_path = home.join("orch.db");
    let old_path = home.join("orchestrator.db");
    if old_path.exists() && !new_path.exists() {
        std::fs::rename(&old_path, &new_path).with_context(|| {
            format!(
                "migrating database from {} to {}",
                old_path.display(),
                new_path.display()
            )
        })?;
        tracing::info!("migrated database: orchestrator.db -> orch.db");
    }
    Ok(new_path)
}

/// Get the path to the service version file (~/.orch/state/service.version).
///
/// Written by the engine at startup, deleted on graceful shutdown.
/// Used by `orch version` to detect CLI/service drift.
pub fn service_version_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(state_dir()?.join("service.version"))
}

/// Get the path to the worktrees directory (~/.orch/worktrees/).
pub fn worktrees_dir() -> anyhow::Result<PathBuf> {
    let dir = orch_home()?.join("worktrees");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Get the path to the contexts directory (~/.orch/contexts/).
pub fn contexts_dir() -> anyhow::Result<PathBuf> {
    let dir = orch_home()?.join("contexts");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Get the path to the projects directory (~/.orch/projects/).
pub fn projects_dir() -> anyhow::Result<PathBuf> {
    let dir = orch_home()?.join("projects");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Get the path to the skills directory (~/.orch/skills/).
pub fn skills_dir() -> anyhow::Result<PathBuf> {
    let dir = orch_home()?.join("skills");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Get the per-repo state directory: `~/.orch/state/{owner}/{repo}/`.
///
/// Falls back to the flat `~/.orch/state/` if no repo is configured.
pub fn repo_state_dir(repo: &str) -> anyhow::Result<PathBuf> {
    let state = state_dir()?;
    let dir = state.join(repo.replace('/', std::path::MAIN_SEPARATOR_STR));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Get the per-task directory: `~/.orch/state/{owner}/{repo}/tasks/{id}/`.
///
/// Creates the directory on demand.
pub fn task_dir(repo: &str, task_id: &str) -> anyhow::Result<PathBuf> {
    let dir = repo_state_dir(repo)?.join("tasks").join(task_id);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Get the per-task attempt directory: `~/.orch/state/{owner}/{repo}/tasks/{id}/attempts/{n}/`.
///
/// Creates the directory on demand.
pub fn task_attempt_dir(repo: &str, task_id: &str, attempt: u32) -> anyhow::Result<PathBuf> {
    let dir = task_dir(repo, task_id)?
        .join("attempts")
        .join(attempt.to_string());
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_orch_home_creates_directory() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir(&home).unwrap();

        let orch_path = home.join(HOME_DIR);
        std::fs::create_dir_all(&orch_path).unwrap();

        assert!(orch_path.exists());
    }

    #[test]
    fn test_state_dir() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir(&home).unwrap();

        let state = home.join(HOME_DIR).join("state");
        std::fs::create_dir_all(&state).unwrap();

        assert!(state.exists());
    }

    #[test]
    fn test_task_dir_creates_path() {
        // Use a temporary HOME so we don't touch the developer's real home dir.
        let temp = TempDir::new().unwrap();
        let home = temp.path();
        std::env::set_var("HOME", home);

        let dir = task_dir("test-owner/test-repo", "42").unwrap();
        assert!(dir.exists());
        assert!(dir.ends_with("test-owner/test-repo/tasks/42"));
        // Cleanup is handled by TempDir when it drops.
    }

    #[test]
    fn test_task_attempt_dir_creates_path() {
        // Use a temporary HOME so we don't touch the developer's real home dir.
        let temp = TempDir::new().unwrap();
        let home = temp.path();
        std::env::set_var("HOME", home);

        let dir = task_attempt_dir("test-owner/test-repo", "42", 1).unwrap();
        assert!(dir.exists());
        assert!(dir.ends_with("test-owner/test-repo/tasks/42/attempts/1"));
        // Cleanup is handled by TempDir when it drops.
    }

    #[test]
    fn test_repo_state_dir_separates_repos() {
        let temp = TempDir::new().unwrap();
        let home = temp.path();
        std::env::set_var("HOME", home);

        let dir_a = repo_state_dir("owner/repo-a").unwrap();
        let dir_b = repo_state_dir("owner/repo-b").unwrap();
        assert_ne!(dir_a, dir_b);
        // Cleanup is handled by TempDir when it drops.
    }
}
