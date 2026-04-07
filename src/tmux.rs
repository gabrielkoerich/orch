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
    /// The task id (preserving internal/external marker), e.g. "internal-21116" or "1775189963034"
    pub task_id: String,
    /// Project short name derived from repo slug (owner/repo -> repo)
    pub project: String,
    pub created_at: Option<u64>, // Unix timestamp from tmux #{session_created}
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
        let safe_task_id = task_id.replace(':', "-");
        format!("{}{project_name}-{safe_task_id}", self.prefix)
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
            if stderr.contains("can't find session") || stderr.contains("no server running") {
                tracing::debug!(session, %stderr, "kill-session: session already gone");
            } else {
                tracing::warn!(session, %stderr, "kill-session failed unexpectedly");
            }
        }
        Ok(())
    }

    /// Capture the current pane content (last N lines).
    ///
    /// Returns `Ok(String::new())` if the session or pane is not found — this is
    /// an expected teardown race when the session was cleaned up between the caller's
    /// `is_session_active()` check and this capture.
    pub async fn capture_pane(&self, session: &str, lines: i32) -> anyhow::Result<String> {
        let start = format!("-{lines}");
        let output = Command::new("tmux")
            .args(["capture-pane", "-t", session, "-p", "-S", &start])
            .output_with_context()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // "can't find pane" / "can't find session" / "no server running" are expected
            // teardown races — the session was cleaned up between the caller's activity
            // check and this call, or the tmux server exited entirely.
            if stderr.contains("can't find pane")
                || stderr.contains("can't find session")
                || stderr.contains("no server running")
            {
                tracing::debug!(
                    session,
                    "capture_pane: session/pane already gone (teardown race)"
                );
                return Ok(String::new());
            }
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

    /// Check whether a session exists and still has a live pane process.
    pub async fn session_is_running(&self, session: &str) -> bool {
        self.session_exists(session).await && self.is_session_active(session).await
    }

    /// Return whether a session should block a new dispatch.
    ///
    /// Dead tmux sessions with the same name are cleaned up eagerly so they do
    /// not block reclaim/re-dispatch loops or cause `new-session` name collisions.
    pub async fn session_blocks_dispatch(&self, session: &str) -> bool {
        if !self.session_exists(session).await {
            return false;
        }

        if self.is_session_active(session).await {
            return true;
        }

        tracing::warn!(
            session,
            "found stale tmux session with dead pane; cleaning up"
        );
        if let Err(err) = self.kill_session(session).await {
            tracing::debug!(session, error = %err, "failed to clean up stale tmux session");
        }

        false
    }

    /// Parse a session name into (project, task_id).
    ///
    /// Session names follow the format `orch-{project}-{task_id}`, where
    /// `{project}` may contain hyphens (e.g. `my-repo`) and `{task_id}` is one of:
    /// - numeric (external): `1234`
    /// - internal: `internal-{n}`
    /// - review variant: `{id}-review` or `internal-{n}-review`
    ///
    /// Parsing works right-to-left since task_id formats are well-defined,
    /// avoiding ambiguity when the project name contains hyphens.
    ///
    /// Returns `None` if the name does not start with the orch prefix or
    /// cannot be parsed into a valid (project, task_id) pair.
    fn parse_session_name(&self, name: &str) -> Option<(String, String)> {
        let after_prefix = name.strip_prefix(&self.prefix)?;

        // Strip optional "-review" suffix, reattach to the task_id later.
        let (base, is_review) = match after_prefix.strip_suffix("-review") {
            Some(stripped) => (stripped, true),
            None => (after_prefix, false),
        };

        // Try internal task: {project}-internal-{digits}
        if let Some(idx) = base.rfind("-internal-") {
            let project = &base[..idx];
            let task_base = &base[idx + 1..]; // "internal-{digits}"
            let after_internal = &task_base["internal-".len()..];
            if !project.is_empty()
                && !after_internal.is_empty()
                && after_internal.chars().all(|c| c.is_ascii_digit())
            {
                let task_id = if is_review {
                    format!("{task_base}-review")
                } else {
                    task_base.to_string()
                };
                return Some((project.to_string(), task_id));
            }
        }

        // External task: {project}-{digits}
        if let Some(idx) = base.rfind('-') {
            let project = &base[..idx];
            let digits = &base[idx + 1..];
            if !project.is_empty()
                && !digits.is_empty()
                && digits.chars().all(|c| c.is_ascii_digit())
            {
                let task_id = if is_review {
                    format!("{digits}-review")
                } else {
                    digits.to_string()
                };
                return Some((project.to_string(), task_id));
            }
        }

        None
    }

    /// Get pane_dead status for all sessions in a single tmux call.
    ///
    /// Returns a map from session name → alive (true = alive, false = dead/missing).
    /// Uses `list-panes -a` to avoid one subprocess per session.
    pub async fn batch_session_active(&self) -> HashMap<String, bool> {
        let output = Command::new("tmux")
            .args(["list-panes", "-a", "-F", "#{session_name} #{pane_dead}"])
            .output()
            .await;
        let mut map = HashMap::new();
        if let Ok(o) = output {
            if o.status.success() {
                for line in String::from_utf8_lossy(&o.stdout).lines() {
                    let mut parts = line.trim().splitn(2, ' ');
                    if let (Some(session), Some(dead)) = (parts.next(), parts.next()) {
                        map.insert(session.to_string(), dead.trim() == "0");
                    }
                }
            }
        }
        map
    }

    /// List all orch-prefixed sessions with metadata including creation time.
    pub async fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
        // Get both name and creation timestamp in one call
        let output = Command::new("tmux")
            .args(["list-sessions", "-F", "#{session_name} #{session_created}"])
            .output_with_context()
            .await?;

        if !output.status.success() {
            return Ok(vec![]);
        }

        let mut sessions = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let parts: Vec<&str> = line.trim().splitn(2, ' ').collect();
            if parts.is_empty() {
                continue;
            }
            let name = parts[0].to_string();
            let created_at = parts.get(1).and_then(|s| s.parse::<u64>().ok());

            if let Some((project, task_id)) = self.parse_session_name(&name) {
                sessions.push(Session {
                    name,
                    task_id,
                    project,
                    created_at,
                });
            }
        }

        Ok(sessions)
    }

    /// Kill all orch-prefixed sessions created before a given timestamp.
    /// Returns the number of sessions killed.
    pub async fn kill_stale_sessions(&self, before_timestamp: u64) -> anyhow::Result<usize> {
        let sessions = self.list_sessions().await?;
        let mut killed = 0;

        for session in sessions {
            let should_kill = match session.created_at {
                Some(created) => created < before_timestamp,
                // If we can't determine creation time, be conservative and don't kill
                None => false,
            };

            if should_kill {
                tracing::info!(
                    session = %session.name,
                    created_at = session.created_at,
                    "killing stale tmux session from previous run"
                );
                if let Err(e) = self.kill_session(&session.name).await {
                    tracing::warn!(session = %session.name, error = %e, "failed to kill stale session");
                } else {
                    killed += 1;
                }
            }
        }

        if killed > 0 {
            tracing::info!(killed, "killed stale tmux sessions on startup");
        }

        Ok(killed)
    }

    /// Wait for multiple sessions to be fully dead (no longer exist).
    /// Polls every `poll_interval` up to `timeout`.
    /// Returns the number of sessions that were still alive after timeout.
    pub async fn wait_for_sessions_dead(
        &self,
        sessions: &[String],
        poll_interval: std::time::Duration,
        timeout: std::time::Duration,
    ) -> usize {
        let start = std::time::Instant::now();
        let mut remaining: std::collections::HashSet<String> = sessions.iter().cloned().collect();

        while !remaining.is_empty() && start.elapsed() < timeout {
            let mut to_remove = Vec::new();
            for session in &remaining {
                if !self.session_exists(session).await {
                    to_remove.push(session.clone());
                }
            }
            for session in to_remove {
                remaining.remove(&session);
            }
            if !remaining.is_empty() {
                tokio::time::sleep(poll_interval).await;
            }
        }

        let still_alive = remaining.len();
        if still_alive > 0 {
            tracing::warn!(
                still_alive,
                sessions = ?remaining.iter().collect::<Vec<_>>(),
                "tmux sessions did not terminate within timeout"
            );
        }
        still_alive
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
                // Process finished — capture final output. `capture_pane` returns empty
                // for "can't find pane/session" teardown races; only unexpected failures
                // reach this error path and are logged at ERROR level.
                match self.capture_pane(session, 500).await {
                    Ok(output) => return Ok(output),
                    Err(err) => {
                        tracing::error!(session = %session, error = %err, "failed to capture final pane output");
                        return Ok(String::new());
                    }
                }
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    /// Snapshot all orch-prefixed sessions — for engine tick monitoring.
    ///
    /// Returns a vector of (Session, alive) so callers can operate on the
    /// actual session name instead of reconstructing one from repo+task_id.
    pub async fn snapshot(&self) -> Vec<(Session, bool)> {
        let sessions = match self.list_sessions().await {
            Ok(s) => s,
            Err(err) => {
                tracing::error!(error = %err, "failed to list tmux sessions — returning empty snapshot");
                Vec::new()
            }
        };
        let mut out = Vec::new();
        for s in sessions {
            let active = self.is_session_active(&s.name).await;
            out.push((s, active));
        }
        out
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

    /// Unset (remove) an environment variable from the tmux **global** environment.
    ///
    /// Uses: `tmux set-environment -gu <key>`
    pub async fn unset_global_env(&self, key: &str) -> anyhow::Result<()> {
        let output = Command::new("tmux")
            .args(["set-environment", "-gu", key])
            .output_with_context()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("tmux unset-global-environment failed for {key}: {stderr}");
        }

        tracing::debug!(key, "unset tmux global environment variable");
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
                .map(|d| d.as_millis())
                .unwrap_or_else(|_| 0)
        )
    }

    #[test]
    fn session_name_internal_task_id_is_sanitized() {
        let tmux = TmuxManager::new();
        assert_eq!(
            tmux.session_name("owner/repo", "internal:8"),
            "orch-repo-internal-8"
        );
    }

    #[test]
    fn task_id_from_session_name_parses_external_task() {
        let tmux = TmuxManager::new();
        // "orch-repo-1234" → project "repo", task_id "1234"
        assert_eq!(
            tmux.parse_session_name("orch-repo-1234"),
            Some(("repo".to_string(), "1234".to_string()))
        );
    }

    #[test]
    fn task_id_from_session_name_parses_internal_task() {
        let tmux = TmuxManager::new();
        // "orch-orch-internal-21116" → project "orch", task_id "internal-21116"
        assert_eq!(
            tmux.parse_session_name("orch-orch-internal-21116"),
            Some(("orch".to_string(), "internal-21116".to_string()))
        );
    }

    #[test]
    fn task_id_from_session_name_parses_internal_task_small_id() {
        let tmux = TmuxManager::new();
        // "orch-repo-internal-8" → project "repo", task_id "internal-8"
        assert_eq!(
            tmux.parse_session_name("orch-repo-internal-8"),
            Some(("repo".to_string(), "internal-8".to_string()))
        );
    }

    #[test]
    fn task_id_from_session_name_returns_none_for_non_orch_session() {
        let tmux = TmuxManager::new();
        assert_eq!(tmux.parse_session_name("some-other-session"), None);
    }

    #[test]
    fn parse_session_name_hyphenated_project_external() {
        let tmux = TmuxManager::new();
        // "orch-my-repo-1234" → project "my-repo", task_id "1234"
        assert_eq!(
            tmux.parse_session_name("orch-my-repo-1234"),
            Some(("my-repo".to_string(), "1234".to_string()))
        );
    }

    #[test]
    fn parse_session_name_hyphenated_project_internal() {
        let tmux = TmuxManager::new();
        // "orch-my-repo-internal-8" → project "my-repo", task_id "internal-8"
        assert_eq!(
            tmux.parse_session_name("orch-my-repo-internal-8"),
            Some(("my-repo".to_string(), "internal-8".to_string()))
        );
    }

    #[test]
    fn parse_session_name_hyphenated_project_review() {
        let tmux = TmuxManager::new();
        // "orch-my-repo-1234-review" → project "my-repo", task_id "1234-review"
        assert_eq!(
            tmux.parse_session_name("orch-my-repo-1234-review"),
            Some(("my-repo".to_string(), "1234-review".to_string()))
        );
    }

    #[test]
    fn parse_session_name_multi_hyphen_project() {
        let tmux = TmuxManager::new();
        // "orch-my-cool-repo-42" → project "my-cool-repo", task_id "42"
        assert_eq!(
            tmux.parse_session_name("orch-my-cool-repo-42"),
            Some(("my-cool-repo".to_string(), "42".to_string()))
        );
    }

    #[test]
    fn parse_session_name_roundtrip_hyphenated_project() {
        let tmux = TmuxManager::new();
        let name = tmux.session_name("owner/my-repo", "1793");
        assert_eq!(name, "orch-my-repo-1793");
        assert_eq!(
            tmux.parse_session_name(&name),
            Some(("my-repo".to_string(), "1793".to_string()))
        );
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

        // Skip test if tmux is not available or fails to create test session.
        match create_result {
            Ok(o) if o.status.success() => {}
            _ => {
                eprintln!("Skipping test: tmux not available or failed to create test session");
                return;
            }
        }

        // Use our helper to set an environment variable
        let result = tmux.set_env(&session, "TEST_VAR", "test_value").await;
        assert!(result.is_ok(), "set_env should succeed");

        // Verify the variable was set by reading it back
        let check_result = tokio::process::Command::new("tmux")
            .args(["show-environment", "-t", &session, "TEST_VAR"])
            .output()
            .await;

        let output = match check_result {
            Ok(o) => o,
            Err(_) => {
                eprintln!("Skipping test: unable to check tmux environment");
                let _ = tmux.kill_session(&session).await;
                return;
            }
        };
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

        match create_result {
            Ok(o) if o.status.success() => {}
            _ => {
                eprintln!("Skipping test: tmux not available or failed to create test session");
                return;
            }
        }

        // First set a variable
        let set_result = tmux.set_env(&session, "TO_DELETE", "temporary").await;
        assert!(set_result.is_ok(), "set_env should succeed");

        // Verify it exists
        let check_before = tokio::process::Command::new("tmux")
            .args(["show-environment", "-t", &session, "TO_DELETE"])
            .output()
            .await;

        let check_before = match check_before {
            Ok(o) => o,
            Err(_) => {
                eprintln!("Skipping test: unable to check tmux environment");
                let _ = tmux.kill_session(&session).await;
                return;
            }
        };

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

    #[tokio::test]
    async fn session_blocks_dispatch_keeps_live_session() {
        let tmux = TmuxManager::new();
        let session = test_session_name();

        let create_result = tokio::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", &session, "-c", "/tmp"])
            .output()
            .await;

        match create_result {
            Ok(o) if o.status.success() => {}
            _ => {
                eprintln!("Skipping test: tmux not available or failed to create test session");
                return;
            }
        }

        assert!(tmux.session_blocks_dispatch(&session).await);
        assert!(tmux.session_exists(&session).await);

        let _ = tmux.kill_session(&session).await;
    }

    #[tokio::test]
    async fn session_blocks_dispatch_cleans_dead_session() {
        let tmux = TmuxManager::new();
        let session = test_session_name();

        let create_result = tokio::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", &session, "-c", "/tmp"])
            .output()
            .await;

        match create_result {
            Ok(o) if o.status.success() => {}
            _ => {
                eprintln!("Skipping test: tmux not available or failed to create test session");
                return;
            }
        }

        let set_option_result = tokio::process::Command::new("tmux")
            .args(["set-option", "-t", &session, "remain-on-exit", "on"])
            .output()
            .await;
        if !matches!(set_option_result, Ok(ref o) if o.status.success()) {
            eprintln!("Skipping test: unable to set tmux remain-on-exit option");
            let _ = tmux.kill_session(&session).await;
            return;
        }

        let send_exit_result = tokio::process::Command::new("tmux")
            .args(["send-keys", "-t", &session, "exit", "Enter"])
            .output()
            .await;
        if !matches!(send_exit_result, Ok(ref o) if o.status.success()) {
            eprintln!("Skipping test: unable to exit tmux pane");
            let _ = tmux.kill_session(&session).await;
            return;
        }

        // Poll until pane is dead (up to 2s) — fixed sleep is unreliable under load.
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if !tmux.session_is_running(&session).await {
                break;
            }
        }

        assert!(tmux.session_exists(&session).await);
        assert!(!tmux.session_is_running(&session).await);
        assert!(!tmux.session_blocks_dispatch(&session).await);
        assert!(!tmux.session_exists(&session).await);
    }

    // NOTE: No static test for GH_TOKEN handling here; behavior is enforced
    // by the call sites (runner code) and covered by session env helper tests.

    /// Regression test: capture_pane returns empty string (not an error) when the
    /// session/pane is already gone — this is an expected teardown race, not an error.
    /// https://github.com/gabrielkoerich/orch/issues/2083
    #[tokio::test]
    async fn capture_pane_returns_empty_for_gone_session() {
        let tmux = TmuxManager::new();
        // Call capture_pane on a session that never existed — should return Ok(""), not an error.
        let result = tmux
            .capture_pane("orch-nonexistent-session-99999", 100)
            .await;
        assert!(
            result.is_ok(),
            "capture_pane should return Ok for non-existent session, got: {:?}",
            result
        );
        assert_eq!(
            result.unwrap(),
            "",
            "capture_pane should return empty string for non-existent session"
        );
    }

    /// Regression test: wait_for_completion returns empty string (not an error) when
    /// the session is already gone.
    /// https://github.com/gabrielkoerich/orch/issues/2083
    #[tokio::test]
    async fn wait_for_completion_returns_empty_for_gone_session() {
        let tmux = TmuxManager::new();
        // Call wait_for_completion on a session that never existed — should return Ok(""), not an error.
        let result = tmux
            .wait_for_completion(
                "orch-nonexistent-session-99999",
                std::time::Duration::from_millis(50),
            )
            .await;
        assert!(
            result.is_ok(),
            "wait_for_completion should return Ok for non-existent session, got: {:?}",
            result
        );
        assert_eq!(
            result.unwrap(),
            "",
            "wait_for_completion should return empty string for non-existent session"
        );
    }
}
