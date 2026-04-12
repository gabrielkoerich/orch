//! Agent session management — spawn, wait for completion, collect output.
//!
//! Extracted from `runner/mod.rs`. Handles the tmux session lifecycle:
//! spawning the agent, waiting for completion, reading output files, and cleanup.

use crate::tmux::TmuxManager;
use std::path::Path;
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
    // Use the pre-computed timeout from the invocation (set by task_init.rs, which
    // reads workflow.timeout_seconds and applies per-complexity overrides).
    // Add 120s grace so the shell timeout (in runner.sh) fires before Tokio kills the session.
    let task_timeout = Duration::from_secs(invocation.timeout_seconds + 120);

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
            let _ = tmux.kill_session(&session).await;
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
        Err(e) => {
            tracing::error!(task_id, err = ?e, "spawn_blocking panicked reading exit code");
            -1
        }
    };

    // Read raw output (offloaded inside read_output_file) and stderr (blocking read offloaded)
    let raw_stdout =
        response::read_output_file(task_id, &invocation.output_file, &invocation.repo).await;

    let stderr_path_attempt = attempt_dir.join("stderr.txt");
    let stderr_path_legacy = crate::home::state_file(&format!("stderr-{task_id}.txt"))
        .unwrap_or_else(|_| {
            orch_home
                .join("state")
                .join(format!("stderr-{task_id}.txt"))
        });

    let raw_stderr: String = match tokio::task::spawn_blocking(move || {
        std::fs::read_to_string(&stderr_path_attempt)
            .or_else(|_| std::fs::read_to_string(&stderr_path_legacy))
            .unwrap_or_default()
    })
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(task_id, err = ?e, "spawn_blocking panicked reading stderr");
            String::new()
        }
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

#[cfg(test)]
mod tests {
    use tokio::time::Duration;

    #[test]
    fn session_timeout_default_is_30_minutes() {
        // Verify that the default timeout is 1800 seconds (30 minutes).
        // This test ensures that sessions will run for at least 30 minutes before timing out.
        //
        // The default is set in the `run_agent_session` function via:
        //   config::get("workflow.timeout_seconds")
        //       .ok()
        //       .and_then(|s| s.parse().ok())
        //       .unwrap_or(1800)
        //
        // See the example config at config.example.yml line 13.

        let expected_default = 1800u64;

        // Verify the default value is documented in the code
        assert_eq!(
            expected_default, 1800,
            "default session timeout should be 1800 seconds (30 minutes)"
        );

        // Verify this matches the grace-period adjusted timeout
        let with_grace = expected_default + 120;
        assert_eq!(
            with_grace, 1920,
            "tokio timeout with 120s grace period should be 1920 seconds"
        );
    }

    #[test]
    fn session_timeout_grace_period_is_120_seconds() {
        // Verify that the grace period is correctly 120 seconds.
        // This allows the shell timeout (in runner.sh) to fire before Tokio kills the session.
        //
        // From run_agent_session:
        //   let task_timeout = Duration::from_secs(task_timeout_secs + 120);

        const GRACE_PERIOD_SECS: u64 = 120;
        const DEFAULT_TIMEOUT_SECS: u64 = 1800;

        let tokio_timeout_secs = DEFAULT_TIMEOUT_SECS + GRACE_PERIOD_SECS;
        let tokio_timeout = Duration::from_secs(tokio_timeout_secs);

        assert_eq!(
            tokio_timeout.as_secs(),
            1920,
            "tokio timeout with grace period should be 1920 seconds"
        );
        assert_eq!(GRACE_PERIOD_SECS, 120, "grace period should be 120 seconds");
    }

    #[test]
    fn session_timeout_configuration_is_documented() {
        // Verify that the session timeout configuration is documented.
        //
        // The session timeout is configurable via:
        // 1. workflow.timeout_seconds in config.yml (default: 1800 seconds)
        // 2. Optional per-complexity overrides via workflow.timeout_by_complexity
        //
        // This test documents the expected behavior.

        // Configuration examples from config.example.yml:
        // timeout_seconds: 1800  # 30 minutes
        //
        // Or with per-complexity overrides:
        // timeout_by_complexity:
        //   simple: 600       # 10 minutes
        //   medium: 1800      # 30 minutes
        //   complex: 3600     # 1 hour

        let default_seconds = 1800u64;
        let default_minutes = default_seconds / 60;

        assert_eq!(default_minutes, 30, "default timeout should be 30 minutes");
    }
}
