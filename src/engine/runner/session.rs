//! Agent session management — spawn, wait for completion, collect output.
//!
//! Extracted from `runner/mod.rs`. Handles the tmux session lifecycle:
//! spawning the agent, waiting for completion, reading output files, and cleanup.

use crate::config;
use crate::tmux::TmuxManager;
use std::path::{Path, PathBuf};
use tokio::time::{timeout, Duration};

use super::{agent, response};

/// Raw output collected from an agent session.
pub struct SessionOutput {
    pub exit_code: i32,
    pub raw_stdout: String,
    pub raw_stderr: String,
    pub elapsed_secs: Option<u64>,
}

/// Spawn the agent in tmux, wait for completion, and collect output.
///
/// Returns `(TmuxManager, session_name, output)`. The `TmuxManager` and
/// session name are needed for cleanup via [`cleanup_session`].
pub async fn run_agent_session(
    task_id: &str,
    invocation: &agent::AgentInvocation,
    attempt_dir: &Path,
    orch_home: &Path,
) -> (TmuxManager, String, SessionOutput) {
    // Read workflow.timeout_seconds from config with default of 1800 (30 minutes)
    let task_timeout_secs: u64 = config::get("workflow.timeout_seconds")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1800);

    // Add 120s grace so the shell timeout (in runner.sh) fires before Tokio kills the session.
    let task_timeout = Duration::from_secs(task_timeout_secs + 120);

    let tmux = TmuxManager::new();

    // Start timing before session setup so elapsed includes spawn/setup time.
    // This helps distinguish "never started" vs "ran for a while then stopped".
    let session_start = std::time::Instant::now();

    let session = match agent::spawn_in_tmux(&tmux, invocation).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(task_id, error = ?e, "failed to spawn agent in tmux");
            return (
                tmux,
                String::new(),
                SessionOutput {
                    exit_code: -1,
                    raw_stdout: String::new(),
                    raw_stderr: e.to_string(),
                    elapsed_secs: None,
                },
            );
        }
    };

    let poll_interval = Duration::from_secs(5);
    let wait_result = timeout(
        task_timeout,
        tmux.wait_for_completion(&session, poll_interval),
    )
    .await;

    let elapsed_since_session_start = session_start.elapsed().as_secs();

    match wait_result {
        Ok(Ok(_output)) => {
            tracing::info!(task_id, "agent session completed");
        }
        Ok(Err(e)) => {
            tracing::error!(task_id, ?e, "error waiting for session");
        }
        Err(_) => {
            tracing::error!(
                task_id,
                elapsed_secs = elapsed_since_session_start,
                "agent timed out after {} seconds",
                elapsed_since_session_start
            );
            let _ = tmux.kill_session(&session).await;
        }
    }

    let output = collect_output(
        task_id,
        invocation,
        attempt_dir,
        orch_home,
        elapsed_since_session_start,
    )
    .await;
    (tmux, session, output)
}

async fn collect_output(
    task_id: &str,
    invocation: &agent::AgentInvocation,
    attempt_dir: &Path,
    orch_home: &Path,
    elapsed_secs: u64,
) -> SessionOutput {
    // Compute exit code on blocking pool
    let attempt_exit = attempt_dir.join("exit.txt");
    let legacy_exit = crate::home::state_file(&format!("exit-{task_id}.txt"))
        .unwrap_or_else(|_| orch_home.join("state").join(format!("exit-{task_id}.txt")));

    let exit_code: i32 = match tokio::task::spawn_blocking(move || {
        std::fs::read_to_string(&attempt_exit)
            .or_else(|_| std::fs::read_to_string(&legacy_exit))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(-1)
    })
    .await
    {
        Ok(code) => code,
        Err(_) => -1,
    };

    // Read raw output (offloaded inside read_output_file) and stderr (blocking read offloaded)
    let raw_stdout = response::read_output_file(task_id, &invocation.output_file, &invocation.repo).await;

    let stderr_path_attempt = attempt_dir.join("stderr.txt");
    let stderr_path_legacy = crate::home::state_file(&format!("stderr-{task_id}.txt"))
        .unwrap_or_else(|_| PathBuf::from(format!("/tmp/stderr-{task_id}.txt")));

    let raw_stderr: String = match tokio::task::spawn_blocking(move || {
        std::fs::read_to_string(&stderr_path_attempt)
            .or_else(|_| std::fs::read_to_string(&stderr_path_legacy))
            .unwrap_or_default()
    })
    .await
    {
        Ok(s) => s,
        Err(_) => String::new(),
    };

    SessionOutput {
        exit_code,
        raw_stdout,
        raw_stderr,
        elapsed_secs: Some(elapsed_secs),
    }
}

/// Kill the tmux session if it still exists, and scrub secrets from the
/// tmux global environment so they never leak across sessions.
pub async fn cleanup_session(task_id: &str, tmux: &TmuxManager, session: &str) {
    if !session.is_empty() && tmux.session_exists(session).await {
        if let Err(e) = tmux.kill_session(session).await {
            tracing::warn!(task_id, error = ?e, "failed to kill tmux session");
        }
    }

    // Scrub GH_TOKEN / GITHUB_TOKEN from the tmux global environment.
    // Per-session vars vanish when the session dies, but a race or bug
    // could leave tokens in the global env — clean up defensively.
    tmux.unset_global_env("GH_TOKEN").await.ok();
    tmux.unset_global_env("GITHUB_TOKEN").await.ok();
}
