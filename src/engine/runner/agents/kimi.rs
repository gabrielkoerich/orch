//! Kimi agent runner — extends the Claude-compatible runner.
//!
//! Kimi uses the `claude` CLI via a shell wrapper that sets
//! `ANTHROPIC_BASE_URL` to the Kimi API. Like MiniMax, the underlying
//! model may not emit `"type":"result"` events consistently.
//!
//! This runner delegates to `MiniMaxRunner` for stream parsing since
//! both use the same claude CLI wrapper pattern and share the same
//! output format differences from native Claude.

use super::minimax::MiniMaxClaudeRunner;
use super::{AgentError, AgentRunner, ParsedResponse, PermissionRules};

/// Runner for Kimi-via-Claude agents (same output handling as MiniMaxClaude).
/// Named `KimiClaudeRunner` to distinguish from a future native Kimi CLI runner.
pub struct KimiClaudeRunner {
    inner: MiniMaxClaudeRunner,
}

impl KimiClaudeRunner {
    pub fn new() -> Self {
        Self {
            inner: MiniMaxClaudeRunner::new("kimi"),
        }
    }
}

impl AgentRunner for KimiClaudeRunner {
    #[cfg(test)]
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn build_command(
        &self,
        model: Option<&str>,
        timeout_cmd: &str,
        sys_file: &str,
        msg_file: &str,
        permissions: &PermissionRules,
    ) -> String {
        self.inner
            .build_command(model, timeout_cmd, sys_file, msg_file, permissions)
    }

    fn parse_response(&self, raw: &str) -> Result<ParsedResponse, AgentError> {
        self.inner.parse_response(raw)
    }

    fn extract_text(&self, raw: &str) -> Result<String, AgentError> {
        self.inner.extract_text(raw)
    }

    fn classify_error(&self, exit_code: i32, stdout: &str, stderr: &str) -> AgentError {
        self.inner.classify_error(exit_code, stdout, stderr)
    }

    fn router_command(
        &self,
        prompt: &str,
        model: Option<&str>,
    ) -> anyhow::Result<tokio::process::Command> {
        self.inner.router_command(prompt, model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kimi_name() {
        let runner = KimiClaudeRunner::new();
        assert_eq!(runner.name(), "kimi");
    }
}
