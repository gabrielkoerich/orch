//! Task runner — delegates to `run_task.sh` for agent execution.
//!
//! Phase 2 approach: the Rust engine owns the loop and concurrency,
//! but `run_task.sh` still handles the complex agent invocation flow
//! (prompt building, git workflow, worktree management, response parsing).
//!
//! The runner spawns `run_task.sh $TASK_ID` as a subprocess and monitors it.
//! This lets us incrementally migrate — bash scripts work unchanged while
//! Rust takes over orchestration.

use anyhow::Context;
use std::path::PathBuf;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

/// Runs tasks by delegating to `run_task.sh`.
pub struct TaskRunner {
    /// Repository slug (owner/repo)
    repo: String,
    /// Path to the scripts directory
    scripts_dir: PathBuf,
    /// Path to the orchestrator home directory
    orch_home: PathBuf,
}

impl TaskRunner {
    pub fn new(repo: String) -> Self {
        let orch_home = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".orchestrator");

        // Scripts live in libexec (brew) or the project dir.
        // Priority: ORCH_SCRIPTS_DIR env > brew --prefix libexec > ORCH_HOME/scripts
        let scripts_dir = std::env::var("ORCH_SCRIPTS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                // Try brew prefix (works on both Apple Silicon and Intel)
                let brew_prefix = std::env::var("HOMEBREW_PREFIX")
                    .unwrap_or_else(|_| "/opt/homebrew".to_string());
                let brew_libexec = PathBuf::from(brew_prefix)
                    .join("opt")
                    .join("orch")
                    .join("libexec")
                    .join("scripts");
                if brew_libexec.exists() {
                    return brew_libexec;
                }
                // Fall back to ORCH_HOME/scripts
                orch_home.join("scripts")
            });

        Self {
            repo,
            scripts_dir,
            orch_home,
        }
    }

    /// Run a task by delegating to `run_task.sh`.
    ///
    /// This spawns the bash script and waits for it to complete.
    /// The script handles everything: routing, worktree, agent invocation,
    /// response parsing, git push, PR creation, and GitHub comments.
    /// Task timeout — 30 minutes. If run_task.sh doesn't finish by then,
    /// we kill it and release the semaphore permit.
    const TASK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

    pub async fn run(&self, task_id: &str) -> anyhow::Result<()> {
        let script = self.scripts_dir.join("run_task.sh");

        if !script.exists() {
            anyhow::bail!("run_task.sh not found at {}", script.display());
        }

        tracing::info!(task_id, script = %script.display(), "spawning run_task.sh");

        // Use Stdio::inherit() — run_task.sh handles its own output
        // (GitHub comments, logging, git). Piped I/O would deadlock if the
        // child writes more than the OS pipe buffer (~64KB) before we read.
        let mut child = Command::new("bash")
            .arg(&script)
            .arg(task_id)
            .env("ORCH_HOME", &self.orch_home)
            .env("GH_REPO", &self.repo)
            .env("PROJECT_DIR", self.project_dir()?)
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .context("spawning run_task.sh")?;

        // Wait with timeout — kill the child if it exceeds the limit
        let status = match timeout(Self::TASK_TIMEOUT, child.wait()).await {
            Ok(result) => result.context("waiting for run_task.sh")?,
            Err(_) => {
                tracing::error!(
                    task_id,
                    timeout_secs = 30 * 60,
                    "run_task.sh timed out, killing"
                );
                child.kill().await.ok();
                anyhow::bail!("run_task.sh timed out after 30 minutes");
            }
        };

        if !status.success() {
            let code = status.code().unwrap_or(-1);
            anyhow::bail!("run_task.sh exited with code {code}");
        }

        Ok(())
    }

    /// Resolve the project directory for this repo.
    ///
    /// Priority: PROJECT_DIR env > bare clone > cwd > error.
    /// Does NOT hardcode ~/Projects/ — that's user-specific.
    fn project_dir(&self) -> anyhow::Result<String> {
        // Explicit env var always wins
        if let Ok(dir) = std::env::var("PROJECT_DIR") {
            if !dir.is_empty() {
                return Ok(dir);
            }
        }

        // Check for bare clone in ORCH_HOME/projects/owner/repo.git
        let parts: Vec<&str> = self.repo.split('/').collect();
        if parts.len() == 2 {
            let bare = self
                .orch_home
                .join("projects")
                .join(parts[0])
                .join(format!("{}.git", parts[1]));
            if bare.exists() {
                return Ok(bare.to_string_lossy().to_string());
            }
        }

        // Fall back to current directory
        Ok(std::env::current_dir()?.to_string_lossy().to_string())
    }
}
