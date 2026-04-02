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

/// Maximum size of raw agent output to load into memory (512 KB).
///
/// Agents that produce outputs larger than this are considered to have
/// gone runaway. The output is truncated to protect memory and prevent
/// downstream parsing issues. The budget check in `handle_success` will
/// catch excessive token usage before any git operations.
const MAX_OUTPUT_SIZE_BYTES: usize = 512 * 1024;

/// Read a file with a size limit. Returns the content truncated to
/// `MAX_OUTPUT_SIZE_BYTES` if the file is larger.
fn read_file_capped(path: &std::path::Path) -> String {
    // Check file size before reading to avoid loading huge files.
    if let Ok(metadata) = std::fs::metadata(path) {
        if metadata.len() > MAX_OUTPUT_SIZE_BYTES as u64 {
            tracing::warn!(
                path = %path.display(),
                size_bytes = metadata.len(),
                limit_bytes = MAX_OUTPUT_SIZE_BYTES,
                "output file exceeds size limit, reading capped portion"
            );
            // Read only the first MAX_OUTPUT_SIZE_BYTES bytes.
            let mut file = std::fs::File::open(path).ok();
            let mut buf = Vec::with_capacity(MAX_OUTPUT_SIZE_BYTES);
            if let Some(ref mut f) = file {
                use std::io::Read;
                let _ = f.take(MAX_OUTPUT_SIZE_BYTES as u64).read_to_end(&mut buf);
            }
            // Ensure valid UTF-8 by walking back to a char boundary.
            return String::from_utf8_lossy(&buf).into_owned();
        }
    }
    std::fs::read_to_string(path).unwrap_or_default()
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

    let exit_code: i32 = (tokio::task::spawn_blocking(move || {
        std::fs::read_to_string(&attempt_exit)
            .or_else(|_| std::fs::read_to_string(&legacy_exit))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(-1)
    })
    .await)
        .unwrap_or(-1);

    // Read raw output with size cap (offloaded inside read_output_file) and stderr (blocking read offloaded)
    let raw_stdout =
        response::read_output_file(task_id, &invocation.output_file, &invocation.repo).await;

    // Enforce hard cap on stdout size after reading.
    let raw_stdout = if raw_stdout.len() > MAX_OUTPUT_SIZE_BYTES {
        tracing::warn!(
            task_id,
            stdout_bytes = raw_stdout.len(),
            limit_bytes = MAX_OUTPUT_SIZE_BYTES,
            "raw stdout exceeds size limit, truncating"
        );
        truncate_to_utf8_boundary(&raw_stdout, MAX_OUTPUT_SIZE_BYTES)
    } else {
        raw_stdout
    };

    let stderr_path_attempt = attempt_dir.join("stderr.txt");
    let stderr_path_legacy = crate::home::state_file(&format!("stderr-{task_id}.txt"))
        .unwrap_or_else(|_| PathBuf::from(format!("/tmp/stderr-{task_id}.txt")));

    let raw_stderr: String = (tokio::task::spawn_blocking(move || {
        let s = read_file_capped(&stderr_path_attempt);
        if s.is_empty() {
            read_file_capped(&stderr_path_legacy)
        } else {
            s
        }
    })
    .await)
        .unwrap_or_default();

    // Enforce hard cap on stderr size after reading.
    let raw_stderr = if raw_stderr.len() > MAX_OUTPUT_SIZE_BYTES {
        truncate_to_utf8_boundary(&raw_stderr, MAX_OUTPUT_SIZE_BYTES)
    } else {
        raw_stderr
    };

    SessionOutput {
        exit_code,
        raw_stdout,
        raw_stderr,
        elapsed_secs: Some(elapsed_secs),
    }
}

/// Truncate `s` to at most `max_bytes`, ensuring the result ends on a valid
/// UTF-8 character boundary.
fn truncate_to_utf8_boundary(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
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
    use super::*;

    #[test]
    fn truncate_to_utf8_boundary_short_string() {
        let s = "hello";
        assert_eq!(truncate_to_utf8_boundary(s, 100), "hello");
        assert_eq!(truncate_to_utf8_boundary(s, 5), "hello");
    }

    #[test]
    fn truncate_to_utf8_boundary_ascii_truncation() {
        let s = "abcdefghij";
        assert_eq!(truncate_to_utf8_boundary(s, 5), "abcde");
        assert_eq!(truncate_to_utf8_boundary(s, 0), "");
    }

    #[test]
    fn truncate_to_utf8_boundary_multibyte() {
        // "日本語" = 3 chars × 3 bytes = 9 bytes
        let s = "日本語";
        assert_eq!(truncate_to_utf8_boundary(s, 9), "日本語");
        // 8 bytes lands in middle of last char — should walk back to 6
        let truncated = truncate_to_utf8_boundary(s, 8);
        assert_eq!(truncated, "日本");
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    #[test]
    fn truncate_to_utf8_boundary_mixed() {
        let s = "hello日本語world";
        let truncated = truncate_to_utf8_boundary(s, 8);
        assert_eq!(truncated, "hello日");
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    #[test]
    fn max_output_size_constant() {
        // Verify the constant is a reasonable size (512 KB)
        assert_eq!(MAX_OUTPUT_SIZE_BYTES, 512 * 1024);
    }
}
