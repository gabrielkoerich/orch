//! tmux session manager — create, monitor, and interact with agent sessions.
//!
//! Each task gets a tmux session named `orch-{issue_number}`.
//! The engine creates sessions, monitors them, captures output for streaming,
//! and cleans them up when tasks complete.
//!
//! This module is the foundation for live session streaming to any channel.

use anyhow::Context;
use std::collections::HashMap;
use tokio::process::Command;

/// Info about an active orch tmux session.
#[derive(Debug, Clone)]
pub struct Session {
    pub name: String,
    pub task_id: String,
    #[allow(dead_code)] // populated for future session monitoring
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[allow(dead_code)]
    pub pane_pid: Option<u32>,
}

/// Manage tmux sessions for agent tasks.
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

    /// Check if tmux server is running.
    #[allow(dead_code)]
    pub async fn is_server_running(&self) -> bool {
        Command::new("tmux")
            .args(["list-sessions"])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Create a new tmux session for a task and run a command in it.
    ///
    /// The session is detached — the agent runs in the background.
    /// Returns the session name.
    #[allow(dead_code)]
    pub async fn create_session(
        &self,
        repo: &str,
        task_id: &str,
        working_dir: &str,
        command: &str,
    ) -> anyhow::Result<String> {
        let name = self.session_name(repo, task_id);

        let output = Command::new("tmux")
            .args([
                "new-session",
                "-d", // detached
                "-s",
                &name, // session name
                "-c",
                working_dir,
                command,
            ])
            .output()
            .await
            .context("spawning tmux session")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("tmux new-session failed: {stderr}");
        }

        tracing::info!(session = %name, task_id, "created tmux session");
        Ok(name)
    }

    /// Check if a session exists.
    pub async fn session_exists(&self, session: &str) -> bool {
        Command::new("tmux")
            .args(["has-session", "-t", session])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Kill a session.
    pub async fn kill_session(&self, session: &str) -> anyhow::Result<()> {
        let output = Command::new("tmux")
            .args(["kill-session", "-t", session])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(session, %stderr, "kill-session failed (may already be dead)");
        }
        Ok(())
    }

    /// Capture the current pane content (last N lines).
    #[allow(dead_code)]
    pub async fn capture_pane(&self, session: &str, lines: i32) -> anyhow::Result<String> {
        let start = format!("-{lines}");
        let output = Command::new("tmux")
            .args(["capture-pane", "-t", session, "-p", "-S", &start])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("capture-pane failed for {session}: {stderr}");
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Send a command line to a session (literal text + Enter).
    ///
    /// Uses `-l` flag so tmux treats the text literally — no interpretation
    /// of special key names like `C-c`, `C-d`, etc. This prevents injection
    /// if untrusted input ever reaches this method.
    #[allow(dead_code)]
    pub async fn send_keys(&self, session: &str, keys: &str) -> anyhow::Result<()> {
        // Send literal text first
        let output = Command::new("tmux")
            .args(["send-keys", "-t", session, "-l", keys])
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("send-keys failed for {session}: {stderr}");
        }
        // Then send Enter separately (as a key name, not literal)
        let output = Command::new("tmux")
            .args(["send-keys", "-t", session, "Enter"])
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("send-keys Enter failed for {session}: {stderr}");
        }
        Ok(())
    }

    /// Send literal text (no Enter) — for piping input.
    #[allow(dead_code)]
    pub async fn send_text(&self, session: &str, text: &str) -> anyhow::Result<()> {
        let output = Command::new("tmux")
            .args(["send-keys", "-t", session, "-l", text])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("send-text failed for {session}: {stderr}");
        }
        Ok(())
    }

    /// Get the PID of the process running in a session's pane.
    #[allow(dead_code)]
    pub async fn pane_pid(&self, session: &str) -> anyhow::Result<Option<u32>> {
        let output = Command::new("tmux")
            .args(["list-panes", "-t", session, "-F", "#{pane_pid}"])
            .output()
            .await?;

        if !output.status.success() {
            return Ok(None);
        }

        let pid_str = String::from_utf8_lossy(&output.stdout);
        Ok(pid_str.trim().parse::<u32>().ok())
    }

    /// Check if the process in a session's pane is still running.
    /// Returns false if the session doesn't exist or the pane has no active process.
    pub async fn is_session_active(&self, session: &str) -> bool {
        // Check pane_dead flag
        let output = Command::new("tmux")
            .args(["list-panes", "-t", session, "-F", "#{pane_dead}"])
            .output()
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
            .args([
                "list-sessions",
                "-F",
                "#{session_name}\t#{session_created}\t#{pane_pid}",
            ])
            .output()
            .await?;

        if !output.status.success() {
            return Ok(vec![]);
        }

        let mut sessions = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 2 && parts[0].starts_with(&self.prefix) {
                let name = parts[0].to_string();
                // Extract task_id: "orch-{project}-{id}" → last segment after final '-'
                let after_prefix = name.strip_prefix(&self.prefix).unwrap_or("");
                let task_id = after_prefix
                    .rsplit('-')
                    .next()
                    .unwrap_or(after_prefix)
                    .to_string();
                let created_ts = parts[1].parse::<i64>().unwrap_or(0);
                let pane_pid = parts.get(2).and_then(|p| p.parse().ok());

                sessions.push(Session {
                    name,
                    task_id,
                    created_at: chrono::DateTime::from_timestamp(created_ts, 0).unwrap_or_default(),
                    pane_pid,
                });
            }
        }

        Ok(sessions)
    }

    /// Wait for a session to finish (pane process exits).
    /// Returns the captured output from the last N lines.
    #[allow(dead_code)]
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
}
