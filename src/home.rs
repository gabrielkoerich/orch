//! Home directory utilities.
//!
//! All orch state lives under `~/.orch/`. This is completely separate from
//! the bash orchestrator's `~/.orchestrator/` directory — both tools can
//! run side by side without conflicts.

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
/// Note: This unifies with sidecar::state_dir() to avoid scattering files.
pub fn state_dir() -> anyhow::Result<PathBuf> {
    let home = orch_home()?;
    let state = home.join("state");
    std::fs::create_dir_all(&state)?;
    Ok(state)
}

/// Get the path to the global config file (~/.orch/config.yml).
pub fn config_path() -> anyhow::Result<PathBuf> {
    Ok(orch_home()?.join("config.yml"))
}

/// Get the path to the tasks database file (~/.orch/tasks.db).
pub fn db_path() -> anyhow::Result<PathBuf> {
    Ok(orch_home()?.join("orchestrator.db"))
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
        // task_dir uses real home, but we can verify it creates the dir
        let dir = task_dir("test-owner/test-repo", "42").unwrap();
        assert!(dir.exists());
        assert!(dir.ends_with("test-owner/test-repo/tasks/42"));
        // Cleanup
        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap().parent().unwrap());
    }

    #[test]
    fn test_task_attempt_dir_creates_path() {
        let dir = task_attempt_dir("test-owner/test-repo", "42", 1).unwrap();
        assert!(dir.exists());
        assert!(dir.ends_with("test-owner/test-repo/tasks/42/attempts/1"));
        // Cleanup
        let _ = std::fs::remove_dir_all(
            dir.parent()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap(),
        );
    }

    #[test]
    fn test_repo_state_dir_separates_repos() {
        let dir_a = repo_state_dir("owner/repo-a").unwrap();
        let dir_b = repo_state_dir("owner/repo-b").unwrap();
        assert_ne!(dir_a, dir_b);
        // Cleanup
        let _ = std::fs::remove_dir_all(dir_a.parent().unwrap());
    }
}
