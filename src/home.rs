//! Home directory utilities with backward compatibility.
//!
//! This module provides a centralized way to get the orch home directory path.
//! It handles the migration from ~/.orchestrator/ to ~/.orch/ with backward compatibility:
//! - If ~/.orch/ exists, use it
//! - If ~/.orchestrator/ exists but ~/.orch/ doesn't, create a symlink
//! - Otherwise, create ~/.orch/

use std::path::PathBuf;

/// The new home directory name.
const NEW_DIR: &str = ".orch";

/// The old home directory name (for backward compatibility).
const OLD_DIR: &str = ".orchestrator";

/// Get the orch home directory path (~/.orch/).
///
/// This function handles backward compatibility:
/// - If ~/.orch/ exists, returns it
/// - If ~/.orchestrator/ exists but ~/.orch/ doesn't, creates a symlink and returns ~/.orch/
/// - Otherwise, creates ~/.orch/ and returns it
pub fn orch_home() -> anyhow::Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    let new_path = home.join(NEW_DIR);
    let old_path = home.join(OLD_DIR);

    // If new path exists, use it
    if new_path.exists() {
        return Ok(new_path);
    }

    // If old path exists but new doesn't, create a symlink for backward compatibility
    if old_path.exists() {
        // Try to create a symlink from new to old
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            if let Err(e) = symlink(&old_path, &new_path) {
                tracing::warn!(
                    old_path = %old_path.display(),
                    new_path = %new_path.display(),
                    error = %e,
                    "failed to create symlink for backward compatibility"
                );
                // Fall back to using the old path directly
                return Ok(old_path);
            }
            tracing::info!(
                old_path = %old_path.display(),
                new_path = %new_path.display(),
                "created symlink for backward compatibility"
            );
        }
        #[cfg(not(unix))]
        {
            // On non-Unix systems, just use the old path
            return Ok(old_path);
        }
    }

    // Create the new directory
    std::fs::create_dir_all(&new_path)?;
    Ok(new_path)
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
#[allow(dead_code)]
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

/// Alias for state_dir (backward compat).
#[allow(dead_code)]
pub fn sidecar_dir() -> anyhow::Result<PathBuf> {
    state_dir()
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

        // Temporarily override home dir
        let new_path = home.join(NEW_DIR);
        std::fs::create_dir_all(&new_path).unwrap();

        assert!(new_path.exists());
    }

    #[test]
    fn test_state_dir() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir(&home).unwrap();

        let state = home.join(NEW_DIR).join("state");
        std::fs::create_dir_all(&state).unwrap();

        assert!(state.exists());
    }
}
