//! Safe wrapper for the `gh` CLI.
//!
//! Provides a centralized way to execute `gh` commands with:
//! - Automatic discovery of the `gh` binary (with launchd fallbacks)
//! - Timeout handling
//! - Structured error types
//! - Logging of all commands
//!
//! Where API parity exists, prefer using [`GhHttp`](super::http::GhHttp) instead.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

use thiserror::Error;

/// Known locations to search for `gh` binary.
const GH_PATHS: &[&str] = &[
    "gh",
    "/opt/homebrew/bin/gh",
    "/usr/local/bin/gh",
    "/usr/bin/gh",
];

/// Default timeout for gh commands.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Errors that can occur when running gh commands.
#[derive(Error, Debug)]
pub enum GhError {
    #[error("gh CLI not found in PATH or known locations")]
    NotFound,

    #[error("gh command timed out after {0}s: {1}")]
    Timeout(u64, String),

    #[error("gh command failed with exit code {0}: {1}")]
    Failed(i32, String),

    #[error("failed to execute gh: {0}")]
    Execution(#[from] std::io::Error),

    #[allow(dead_code)]
    #[error("gh command produced invalid UTF-8: {0}")]
    InvalidUtf8(String),
}

/// Result type for gh CLI operations.
pub type GhResult<T> = Result<T, GhError>;

/// Builder for running gh CLI commands.
pub struct Gh {
    args: Vec<String>,
    timeout: Duration,
    current_dir: Option<PathBuf>,
    /// Additional environment variables to set.
    env: Vec<(String, String)>,
}

impl Gh {
    /// Create a new gh command builder.
    pub fn new(args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            args: args.into_iter().map(|a| a.into()).collect(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            current_dir: None,
            env: Vec::new(),
        }
    }

    /// Set a timeout for the command.
    pub fn timeout(mut self, duration: Duration) -> Self {
        self.timeout = duration;
        self
    }

    /// Set the working directory for the command.
    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(dir.into());
        self
    }

    /// Add an environment variable.
    #[allow(dead_code)]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Execute the command and return the output (sync version without timeout).
    /// For timeout support, use the async version.
    pub fn output(&self) -> GhResult<Output> {
        let gh_path = find_gh()?;
        tracing::debug!(path = %gh_path.display(), args = ?self.args, "executing gh command");

        let mut cmd = Command::new(&gh_path);
        cmd.args(&self.args);

        if let Some(ref dir) = self.current_dir {
            cmd.current_dir(dir);
        }

        // Add environment variables (preserve existing env)
        for (key, value) in &self.env {
            cmd.env(key, value);
        }

        match cmd.output() {
            Ok(output) => {
                if output.status.success() {
                    tracing::debug!(
                        path = %gh_path.display(),
                        args = ?self.args,
                        "gh command succeeded"
                    );
                    Ok(output)
                } else {
                    let exit_code = output.status.code().unwrap_or(-1);
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::warn!(
                        path = %gh_path.display(),
                        args = ?self.args,
                        exit_code,
                        stderr = %stderr,
                        "gh command failed"
                    );
                    Err(GhError::Failed(exit_code, stderr.into_owned()))
                }
            }
            Err(e) => {
                tracing::error!(
                    path = %gh_path.display(),
                    args = ?self.args,
                    error = %e,
                    "gh command execution failed"
                );
                Err(GhError::Execution(e))
            }
        }
    }

    /// Execute the command asynchronously and return the output.
    pub async fn output_async(&self) -> GhResult<Output> {
        use tokio::process::Command as TokioCommand;

        let gh_path = find_gh()?;
        tracing::debug!(path = %gh_path.display(), args = ?self.args, "executing gh command (async)");

        let mut cmd = TokioCommand::new(&gh_path);
        cmd.args(&self.args);

        if let Some(ref dir) = self.current_dir {
            cmd.current_dir(dir);
        }

        // Add environment variables
        for (key, value) in &self.env {
            cmd.env(key, value);
        }

        // Use timeout
        match tokio::time::timeout(self.timeout, cmd.output()).await {
            Ok(Ok(output)) => {
                if output.status.success() {
                    tracing::debug!(
                        path = %gh_path.display(),
                        args = ?self.args,
                        "gh command succeeded"
                    );
                    Ok(output)
                } else {
                    let exit_code = output.status.code().unwrap_or(-1);
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::warn!(
                        path = %gh_path.display(),
                        args = ?self.args,
                        exit_code,
                        stderr = %stderr,
                        "gh command failed"
                    );
                    Err(GhError::Failed(exit_code, stderr.into_owned()))
                }
            }
            Ok(Err(e)) => Err(GhError::Execution(e)),
            Err(_) => {
                tracing::warn!(
                    path = %gh_path.display(),
                    args = ?self.args,
                    timeout_secs = self.timeout.as_secs(),
                    "gh command timed out"
                );
                Err(GhError::Timeout(
                    self.timeout.as_secs(),
                    format!("{:?}", self.args),
                ))
            }
        }
    }
}

/// Find the gh binary path.
fn find_gh() -> GhResult<PathBuf> {
    // First try PATH lookup
    if let Ok(path) = which_gh() {
        return Ok(path);
    }

    // Then try known fallback paths
    for path in GH_PATHS {
        let p = PathBuf::from(path);
        if p.exists() {
            tracing::debug!(path = %p.display(), "found gh at fallback path");
            return Ok(p);
        }
    }

    tracing::error!("gh CLI not found in PATH or known locations");
    Err(GhError::NotFound)
}

/// Try to find gh in PATH using `which` or `where`.
fn which_gh() -> GhResult<PathBuf> {
    // Try using std::process::Command to find gh
    let output = Command::new("which")
        .arg("gh")
        .output()
        .map_err(GhError::Execution)?;

    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }

    // On Windows, try `where`
    #[cfg(windows)]
    {
        let output = Command::new("where")
            .arg("gh")
            .output()
            .map_err(GhError::Execution)?;

        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(PathBuf::from(path.split('\n').next().unwrap_or(&path)));
            }
        }
    }

    // Last resort: try running `gh --version` to see if it's in PATH
    let output = Command::new("gh")
        .args(["--version"])
        .output()
        .map_err(GhError::Execution)?;

    if output.status.success() {
        // gh is in PATH but we couldn't get the path - return generic
        return Ok(PathBuf::from("gh"));
    }

    Err(GhError::NotFound)
}

/// Check if gh is available on the system.
#[allow(dead_code)]
pub fn is_available() -> bool {
    find_gh().is_ok()
}

/// Resolve GitHub token using the same priority as GhHttp:
///
/// 1. `GH_TOKEN` environment variable
/// 2. `GITHUB_TOKEN` environment variable
/// 3. `gh auth token` CLI command
pub fn resolve_token() -> Option<String> {
    // Priority 1: GH_TOKEN env var
    if let Ok(t) = std::env::var("GH_TOKEN") {
        if !t.is_empty() {
            return Some(t);
        }
    }

    // Priority 2: GITHUB_TOKEN env var
    if let Ok(t) = std::env::var("GITHUB_TOKEN") {
        if !t.is_empty() {
            return Some(t);
        }
    }

    // Priority 3: gh auth token (using our wrapper)
    let gh = Gh::new(["auth", "token"]).timeout(Duration::from_secs(10));
    if let Ok(output) = gh.output() {
        if output.status.success() {
            let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !token.is_empty() {
                return Some(token);
            }
        }
    }

    tracing::warn!("gh auth token not available; GitHub operations may fail");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gh_not_found_returns_error() {
        // This test verifies error handling when gh is not available
        // In CI/environments without gh, should return NotFound
        let result = Gh::new(["--version"]).output();
        // Either succeeds (gh available) or returns NotFound
        match result {
            Ok(output) => {
                assert!(output.status.success());
                let version = String::from_utf8_lossy(&output.stdout);
                assert!(version.contains("gh version"));
            }
            Err(GhError::NotFound) => {
                // Expected when gh is not installed
            }
            Err(e) => {
                // Other errors are acceptable in test environments
                tracing::debug!("gh returned error in test: {}", e);
            }
        }
    }

    #[test]
    fn test_gh_auth_token() {
        let token = resolve_token();
        if token.is_some() {
            // Token should be a reasonable length
            assert!(token.unwrap().len() >= 10);
        }
    }

    #[test]
    fn test_gh_args_escaping() {
        let gh = Gh::new(["pr", "list", "--head", "feature/test"]);
        assert_eq!(gh.args, vec!["pr", "list", "--head", "feature/test"]);
    }

    #[tokio::test]
    async fn test_gh_async() {
        let gh = Gh::new(["auth", "status"]).timeout(Duration::from_secs(10));
        let result = gh.output_async().await;
        // Either succeeds or fails gracefully
        match result {
            Ok(output) => {
                // auth status may fail if not logged in, but should be valid output
                let _ = String::from_utf8_lossy(&output.stdout);
            }
            Err(GhError::NotFound) => {
                // Expected when gh is not installed
            }
            Err(e) => {
                // Other errors (auth not configured, etc.) are acceptable
                tracing::debug!("gh auth status error (expected if not logged in): {}", e);
            }
        }
    }

    #[test]
    fn test_gh_with_invalid_command() {
        let gh = Gh::new(["invalid", "command", "that", "does", "not", "exist"]);
        let result = gh.output();
        assert!(result.is_err());
    }
}
