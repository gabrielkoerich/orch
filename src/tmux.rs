//! tmux session manager — create, monitor, and interact with agent sessions.
//!
//! Each task gets a tmux session named `orch-{issue_number}`.
//! The engine creates sessions, monitors them, captures output for streaming,
//! and cleans them up when tasks complete.
//!
//! This module is the foundation for live session streaming to any channel.

use crate::cmd::CommandErrorContext;
use anyhow::Context;
use std::collections::HashMap;
use tokio::process::Command;

/// Info about an active orch tmux session.
#[derive(Debug, Clone)]
pub struct Session {
    pub name: String,
    pub task_id: String,
}

/// Manage tmux sessions for agent tasks.
#[derive(Clone)]
pub struct TmuxManager {
    /// Prefix for session names (e.g. "orch-")
    prefix: String,
}

impl TmuxManager {
    pub fn new() -> Self {
        Self {
            prefix: "orch-".to_string(),
        }
    }

    /// Session name for a task: `orch-{project}-{task_id}`.
    ///
    /// The project name is derived from the repo slug (e.g. `owner/repo` → `repo`).
    /// This prevents collisions between projects with the same issue number.
    pub fn session_name(&self, project: &str, task_id: &str) -> String {
        let project_name = project
            .rsplit('/')
            .next()
            .unwrap_or(project)
            .trim_end_matches(".git");
        format!("{}{project_name}-{task_id}", self.prefix)
    }

    /// Create a new tmux session for a task and run a command in it.
    ///
    /// Environment variables in `env_vars` are injected into the session via
    /// `tmux new-session -e KEY=VALUE` — they are not written to disk.
    ///
    /// The session is detached — the agent runs in the background.
    /// Returns the session name.
    ///
    /// Note: For secrets like GH_TOKEN, use [`set_env`] after session creation
    /// instead of passing them here. This avoids exposing secrets in process arguments.
    pub async fn create_session(
        &self,
        repo: &str,
        task_id: &str,
        working_dir: &str,
        command: &str,
        env_vars: &[(&str, &str)],
    ) -> anyhow::Result<String> {
        let name = self.session_name(repo, task_id);

        let mut cmd = Command::new("tmux");
        cmd.args(["new-session", "-d", "-s", &name, "-c", working_dir]);

        // Inject non-secret environment variables into the new session if provided.
        // For secrets like GH_TOKEN, callers should use `set_env()` after
        // session creation to avoid exposing them in process arguments.
        for (key, value) in env_vars {
            // tmux accepts -e KEY=VALUE to inject environment into the session.
            cmd.arg("-e").arg(format!("{}={}", key, value));
        }

        cmd.arg(command);

        let output = cmd
            .output_with_context()
            .await
            .context("spawning tmux session")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("tmux new-session failed: {stderr}");
        }

        tracing::info!(session = %name, task_id, "created tmux session");
        Ok(name)
    }

    /// Set an environment variable in an existing tmux session.
    ///
    /// This is preferred over passing secrets via [`create_session`] because
    /// it avoids exposing secrets in process arguments and on-disk runner scripts.
    ///
    /// Uses: `tmux set-environment -t <session> <key> <value>`
    pub async fn set_session_env(
        &self,
        session: &str,
        key: &str,
        value: &str,
    ) -> anyhow::Result<()> {
        self.set_env(session, key, value).await
    }

    /// Unset an environment variable in an existing tmux session.
    ///
    /// Uses: `tmux set-environment -u <key> -t <session>`
    #[allow(dead_code)]
    pub async fn unset_session_env(&self, session: &str, key: &str) -> anyhow::Result<()> {
        self.unset_env(session, key).await
    }

    /// Send literal text into a session's active pane.
    pub async fn send_text(&self, session: &str, text: &str) -> anyhow::Result<()> {
        let output = Command::new("tmux")
            .args(["send-keys", "-t", session, "-l", text])
            .output_with_context()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("send-keys failed for {session}: {stderr}");
        }
        Ok(())
    }

    /// Check if a session exists.
    pub async fn session_exists(&self, session: &str) -> bool {
        Command::new("tmux")
            .args(["has-session", "-t", session])
            .output_with_context()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Kill a session.
    pub async fn kill_session(&self, session: &str) -> anyhow::Result<()> {
        let output = Command::new("tmux")
            .args(["kill-session", "-t", session])
            .output_with_context()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(session, %stderr, "kill-session failed (may already be dead)");
        }
        Ok(())
    }

    /// Capture the current pane content (last N lines).
    pub async fn capture_pane(&self, session: &str, lines: i32) -> anyhow::Result<String> {
        let start = format!("-{lines}");
        let output = Command::new("tmux")
            .args(["capture-pane", "-t", session, "-p", "-S", &start])
            .output_with_context()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("capture-pane failed for {session}: {stderr}");
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Check if the process in a session's pane is still running.
    /// Returns false if the session doesn't exist or the pane has no active process.
    pub async fn is_session_active(&self, session: &str) -> bool {
        // Check pane_dead flag
        let output = Command::new("tmux")
            .args(["list-panes", "-t", session, "-F", "#{pane_dead}"])
            .output_with_context()
            .await;

        match output {
            Ok(o) if o.status.success() => {
                let flag = String::from_utf8_lossy(&o.stdout);
                flag.trim() == "0" // 0 = alive, 1 = dead
            }
            _ => false,
        }
    }

    /// List all orch-prefixed sessions with metadata.
    pub async fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
        let output = Command::new("tmux")
            .args(["list-sessions", "-F", "#{session_name}"])
            .output_with_context()
            .await?;

        if !output.status.success() {
            return Ok(vec![]);
        }

        let mut sessions = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let name = line.trim().to_string();
            if name.starts_with(&self.prefix) {
                // Extract task_id: "orch-{project}-{id}" → last segment after final '-'
                let after_prefix = name.strip_prefix(&self.prefix).unwrap_or("");
                let task_id = after_prefix
                    .rsplit('-')
                    .next()
                    .unwrap_or(after_prefix)
                    .to_string();

                sessions.push(Session { name, task_id });
            }
        }

        Ok(sessions)
    }

    /// Wait for a session to finish (pane process exits).
    /// Returns the captured output from the last N lines.
    pub async fn wait_for_completion(
        &self,
        session: &str,
        poll_interval: std::time::Duration,
    ) -> anyhow::Result<String> {
        loop {
            if !self.session_exists(session).await {
                return Ok(String::new());
            }

            if !self.is_session_active(session).await {
                // Process finished — capture final output
                let output = self.capture_pane(session, 500).await.unwrap_or_default();
                return Ok(output);
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    /// Snapshot all active sessions — for engine tick monitoring.
    pub async fn snapshot(&self) -> HashMap<String, bool> {
        let sessions = self.list_sessions().await.unwrap_or_default();
        let mut map = HashMap::new();
        for s in sessions {
            let active = self.is_session_active(&s.name).await;
            map.insert(s.task_id, active);
        }
        map
    }

    // ── Environment variable helpers ───────────────────────────────────

    /// Set an environment variable in a tmux session.
    ///
    /// This updates the tmux session environment, which will be inherited
    /// by processes started in new windows/panes within the session.
    pub async fn set_env(&self, session: &str, key: &str, value: &str) -> anyhow::Result<()> {
        let output = Command::new("tmux")
            .args(["set-environment", "-t", session, key, value])
            .output_with_context()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("tmux set-environment failed for {session}: {stderr}");
        }

        tracing::debug!(session, key, "set tmux environment variable");
        Ok(())
    }

    /// Unset (remove) an environment variable from a tmux session.
    #[allow(dead_code)]
    pub async fn unset_env(&self, session: &str, key: &str) -> anyhow::Result<()> {
        let output = Command::new("tmux")
            .args(["set-environment", "-t", session, "-u", key])
            .output_with_context()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("tmux unset-environment failed for {session}: {stderr}");
        }

        tracing::debug!(session, key, "unset tmux environment variable");
        Ok(())
    }

    /// Set multiple environment variables in a tmux session at once.
    ///
    /// This is a convenience method for setting multiple variables efficiently.
    #[allow(dead_code)]
    pub async fn set_env_batch(
        &self,
        session: &str,
        vars: &[(String, String)],
    ) -> anyhow::Result<()> {
        for (key, value) in vars {
            self.set_env(session, key, value).await?;
        }
        Ok(())
    }

    /// Set the GitHub token in a tmux session environment.
    ///
    /// This is a convenience method that sets GH_TOKEN (and optionally
    /// GITHUB_TOKEN) for agent sessions.
    pub async fn set_github_token(
        &self,
        session: &str,
        token: &str,
        set_github_token_var: bool,
    ) -> anyhow::Result<()> {
        self.set_env(session, "GH_TOKEN", token).await?;

        if set_github_token_var {
            self.set_env(session, "GITHUB_TOKEN", token).await?;
        }

        tracing::debug!(session, "set GitHub token in tmux session environment");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to get a test session name with unique ID
    fn test_session_name() -> String {
        format!(
            "orch-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        )
    }

    /// Verify set_env runs the correct tmux set-environment command.
    /// This test creates a temporary session, sets an env var, and verifies it was set.
    #[tokio::test]
    async fn test_set_env() {
        let tmux = TmuxManager::new();
        let session = test_session_name();

        // Create a temporary detached session for testing
        let create_result = tokio::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", &session, "-c", "/tmp"])
            .output()
            .await;

        if create_result.is_err() || !create_result.unwrap().status.success() {
            // Skip test if tmux is not available or fails
            eprintln!("Skipping test: tmux not available or failed to create test session");
            return;
        }

        // Use our helper to set an environment variable
        let result = tmux.set_env(&session, "TEST_VAR", "test_value").await;
        assert!(result.is_ok(), "set_env should succeed");

        // Verify the variable was set by reading it back
        let check_result = tokio::process::Command::new("tmux")
            .args(["show-environment", "-t", &session, "TEST_VAR"])
            .output()
            .await;

        let output = check_result.expect("should be able to check environment");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("TEST_VAR=test_value"),
            "Expected TEST_VAR=test_value, got: {}",
            stdout
        );

        // Cleanup: kill the test session
        let _ = tmux.kill_session(&session).await;
    }

    /// Verify unset_env runs the correct tmux set-environment -u command.
    /// This test verifies the function can be called without error.
    /// Note: Full verification of tmux behavior depends on the environment.
    #[tokio::test]
    async fn test_unset_env() {
        let tmux = TmuxManager::new();
        let session = test_session_name();

        // Create a temporary detached session for testing
        let create_result = tokio::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", &session, "-c", "/tmp"])
            .output()
            .await;

        if create_result.is_err() || !create_result.unwrap().status.success() {
            eprintln!("Skipping test: tmux not available or failed to create test session");
            return;
        }

        // First set a variable
        let set_result = tmux.set_env(&session, "TO_DELETE", "temporary").await;
        assert!(set_result.is_ok(), "set_env should succeed");

        // Verify it exists
        let check_before = tokio::process::Command::new("tmux")
            .args(["show-environment", "-t", &session, "TO_DELETE"])
            .output()
            .await
            .expect("should be able to check environment");
        assert!(
            String::from_utf8_lossy(&check_before.stdout).contains("TO_DELETE"),
            "Variable should exist before unset"
        );

        // Call unset_session_env - verify it runs without error
        let unset_result = tmux.unset_env(&session, "TO_DELETE").await;
        assert!(unset_result.is_ok(), "unset_env should succeed");

        // Cleanup: kill the test session
        let _ = tmux.kill_session(&session).await;
    }

    // NOTE: No static test for GH_TOKEN handling here; behavior is enforced
    // by the call sites (runner code) and covered by session env helper tests.
}
